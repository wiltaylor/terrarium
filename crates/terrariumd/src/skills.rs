//! The skill library (PLAN §8.6): `<config repo>/skills/<name>/`, its
//! reads and writes for `/api/skills`, the check of a `skills` list at
//! assignment, and the record of the skills that an agent got at its
//! start.
//!
//! The checks of one skill and the copy into an agent's harness dir are in
//! `terrarium_core::skills`. A write goes to the work tree of the config
//! repo and stays uncommitted, as an edit of a prompt does: the Config
//! page (or `terrarium admin config commit`) commits it, with the secret
//! scan.

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::skills::{self as core, Files};

use crate::error::{ApiError, ApiResult};

/// The record of the delivered skills of the last start, in the agent's
/// `private/` dir.
pub const DELIVERED_FILE: &str = "skills.json";

/// The library dir of the config repo at `repo`.
pub fn library_dir(repo: &Path) -> PathBuf {
    repo.join(core::LIBRARY_DIR)
}

/// The library dir from the environment, for the checks that have no
/// daemon state (the fleet check): `TERRARIUM_CONFIG_REPO`, else
/// `/config/terrarium`.
pub fn library_dir_from_env() -> PathBuf {
    let repo = std::env::var_os("TERRARIUM_CONFIG_REPO")
        .filter(|v| !v.is_empty())
        .map_or_else(
            || PathBuf::from(crate::config_repo::DEFAULT_DIR),
            PathBuf::from,
        );
    library_dir(&repo)
}

/// One skill in `GET /api/skills`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub name: String,
    /// From the front matter. Empty when it does not parse.
    pub description: String,
    pub files: usize,
    pub bytes: usize,
    /// The skill is default on: its front matter has
    /// `metadata.terrarium-default: "true"`. Each agent gets it unless the
    /// agent turns it off (`skills_off`, `default_skills`).
    #[serde(default)]
    pub default_on: bool,
    /// The problems of the checks (`terrarium_core::skills::check`). A
    /// skill with problems is not delivered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// One file of a skill in `GET /api/skills/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileView {
    pub path: String,
    pub size: usize,
    /// The text, for a UTF-8 file other than `SKILL.md`. Absent for a
    /// binary file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `GET /api/skills/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detail {
    #[serde(flatten)]
    pub summary: Summary,
    /// The text of `SKILL.md`.
    pub skill_md: String,
    /// Every file but `SKILL.md`, sorted by path.
    pub file_list: Vec<FileView>,
}

/// The content of a file in a write: text, or `{"base64": "..."}` for a
/// binary file (an image).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FileBody {
    Text(String),
    Base64 { base64: String },
}

impl FileBody {
    pub fn bytes(&self, path: &str) -> ApiResult<Vec<u8>> {
        match self {
            FileBody::Text(t) => Ok(t.clone().into_bytes()),
            FileBody::Base64 { base64 } => base64::engine::general_purpose::STANDARD
                .decode(base64.trim())
                .map_err(|e| ApiError::invalid(format!("`files.{path}`: bad base64: {e}"))),
        }
    }

    /// The body of `bytes`: text when it is UTF-8 with no NUL, else base64.
    pub fn of(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(t) if !bytes.contains(&0) => FileBody::Text(t.to_owned()),
            _ => FileBody::Base64 {
                base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        }
    }
}

/// The body of `POST /api/skills` and `PUT /api/skills/{name}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteBody {
    /// The name (`POST` only; `PUT` takes it from the path).
    #[serde(default)]
    pub name: Option<String>,
    /// The text of `SKILL.md`. `POST` without it, with `description`:
    /// a new `SKILL.md` with that description.
    #[serde(default)]
    pub skill_md: Option<String>,
    /// For a new skill without `skill_md`.
    #[serde(default)]
    pub description: Option<String>,
    /// The other files: path in the skill, then the content. Absent: a
    /// `PUT` keeps the other files as they are. Present: they replace all
    /// other files.
    #[serde(default)]
    pub files: Option<std::collections::BTreeMap<String, FileBody>>,
}

/// A new `SKILL.md` with `name` and `description`.
pub fn new_skill_md(name: &str, description: &str) -> String {
    let d = description.trim().replace('\n', " ");
    let quoted = serde_json::to_string(&d).unwrap_or_else(|_| "\"\"".into());
    format!(
        "---\nname: {name}\ndescription: {quoted}\n---\n\n# {name}\n\nWrite the instructions \
         of the skill here.\n"
    )
}

fn not_found(name: &str) -> ApiError {
    ApiError::not_found(format!("no skill `{name}` in the skill library"))
}

fn check_name(name: &str) -> ApiResult<()> {
    core::check_name(name).map_err(ApiError::invalid)
}

fn summary(name: &str, files: Result<Files, Vec<String>>) -> (Summary, Files) {
    let (files, mut errors) = match files {
        Ok(f) => (f, Vec::new()),
        Err(p) => (Files::new(), p),
    };
    let meta = files
        .get(core::SKILL_FILE)
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(|t| core::parse_front_matter(t).ok());
    let default_on = meta.as_ref().is_some_and(|m| m.default_on);
    let description = meta.map(|m| m.description).unwrap_or_default();
    if errors.is_empty()
        && let Err(p) = core::check(name, &files)
    {
        errors = p;
    }
    (
        Summary {
            name: name.to_owned(),
            description,
            files: files.len(),
            bytes: files.values().map(Vec::len).sum(),
            default_on,
            errors,
        },
        files,
    )
}

/// Every skill of the library, sorted by name, with its problems.
pub fn list(lib: &Path) -> Vec<Summary> {
    core::library_names(lib)
        .into_iter()
        .map(|n| summary(&n, core::read_dir(&lib.join(&n))).0)
        .collect()
}

/// One skill with its files.
pub fn get(lib: &Path, name: &str) -> ApiResult<Detail> {
    check_name(name)?;
    let dir = lib.join(name);
    if !dir.is_dir() {
        return Err(not_found(name));
    }
    let (summary, files) = summary(name, core::read_dir(&dir));
    let skill_md = files
        .get(core::SKILL_FILE)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let file_list = files
        .iter()
        .filter(|(p, _)| p.as_str() != core::SKILL_FILE)
        .map(|(p, b)| FileView {
            path: p.clone(),
            size: b.len(),
            text: match FileBody::of(b) {
                FileBody::Text(t) => Some(t),
                FileBody::Base64 { .. } => None,
            },
        })
        .collect();
    Ok(Detail {
        summary,
        skill_md,
        file_list,
    })
}

/// The files of a write: the body over the files that are there.
fn files_of(lib: &Path, name: &str, b: &WriteBody, create: bool) -> ApiResult<Files> {
    let mut files = if create {
        Files::new()
    } else {
        core::read_dir(&lib.join(name))
            .map_err(|p| ApiError::invalid(format!("cannot read the skill: {}", p.join("; "))))?
    };
    if let Some(new) = &b.files {
        files.retain(|p, _| p == core::SKILL_FILE);
        for (path, body) in new {
            if path == core::SKILL_FILE {
                return Err(ApiError::invalid(
                    "give SKILL.md in `skill_md`, not in `files`",
                ));
            }
            files.insert(path.clone(), body.bytes(path)?);
        }
    }
    match (&b.skill_md, &b.description) {
        (Some(md), _) => {
            files.insert(core::SKILL_FILE.into(), md.clone().into_bytes());
        }
        (None, Some(d)) if create => {
            files.insert(core::SKILL_FILE.into(), new_skill_md(name, d).into_bytes());
        }
        (None, Some(_)) => {
            return Err(ApiError::invalid(
                "`description` makes a new SKILL.md; to change a skill, give `skill_md`",
            ));
        }
        (None, None) if create => {
            return Err(ApiError::invalid(
                "a new skill needs `skill_md`, or `description` for a new SKILL.md",
            ));
        }
        (None, None) => {}
    }
    Ok(files)
}

/// The error of a skill that fails the checks: `422 invalid`, with the
/// problems in `details.problems`.
pub fn invalid(problems: &[String]) -> ApiError {
    ApiError::invalid(format!("the skill is not valid: {}", problems.join("; ")))
        .with_details(json!({ "problems": problems }))
}

/// Write a skill: a new one (`create`, `409` when it exists) or a change
/// (`404` when it does not exist). The checks run first; a skill that
/// fails them is not written. The dir is written beside the old one and
/// then moved over it.
pub fn write(lib: &Path, name: &str, b: &WriteBody, create: bool) -> ApiResult<Detail> {
    check_name(name)?;
    let dir = lib.join(name);
    let exists = dir.exists();
    if create && exists {
        return Err(ApiError::conflict(format!(
            "the skill `{name}` exists already; change it with PUT /api/skills/{name}"
        )));
    }
    if !create && !exists {
        return Err(not_found(name));
    }
    let files = files_of(lib, name, b, create)?;
    core::check(name, &files).map_err(|p| invalid(&p))?;
    std::fs::create_dir_all(lib)
        .map_err(|e| ApiError::internal(format!("{}: {e}", lib.display())))?;
    let tmp = lib.join(format!(".write-{name}-{}", ulid::Ulid::new()));
    core::write_files(&tmp, &files).map_err(ApiError::internal)?;
    if exists {
        let old = lib.join(format!(".old-{name}-{}", ulid::Ulid::new()));
        std::fs::rename(&dir, &old)
            .map_err(|e| ApiError::internal(format!("{}: {e}", dir.display())))?;
        if let Err(e) = std::fs::rename(&tmp, &dir) {
            let _ = std::fs::rename(&old, &dir);
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(ApiError::internal(format!("{}: {e}", dir.display())));
        }
        let _ = std::fs::remove_dir_all(&old);
    } else {
        std::fs::rename(&tmp, &dir)
            .map_err(|e| ApiError::internal(format!("{}: {e}", dir.display())))?;
    }
    get(lib, name)
}

/// Delete a skill. The agents that name it keep the name, and get a
/// warning at their next start.
pub fn remove(lib: &Path, name: &str) -> ApiResult<()> {
    check_name(name)?;
    let dir = lib.join(name);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(&dir),
        Ok(_) => std::fs::remove_file(&dir),
        Err(_) => return Err(not_found(name)),
    }
    .map_err(|e| ApiError::internal(format!("{}: {e}", dir.display())))
}

/// Check a `skills` list at assignment: each name is valid and in the
/// library (`lib`; `None`: the daemon has no config repo). `*` is always
/// valid. An unknown name is `422 invalid`.
pub fn check_assignment(lib: Option<&Path>, list: &[String]) -> ApiResult<()> {
    let list = core::clean_list(list).map_err(ApiError::invalid)?;
    let names: Vec<&String> = list.iter().filter(|n| n.as_str() != core::ALL).collect();
    if names.is_empty() {
        return Ok(());
    }
    let have = lib.map(core::library_names).unwrap_or_default();
    let unknown: Vec<&str> = names
        .iter()
        .filter(|n| !have.contains(n))
        .map(|n| n.as_str())
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(ApiError::invalid(format!(
        "no skill {} in the skill library; add it first (`terrarium skill new`)",
        unknown
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .with_details(json!({ "unknown_skills": unknown })))
}

/// The hash of the skills that the selection `sel` picks from the library
/// now (default-on skills included), for the "restart to apply" check:
/// the same value as the delivery would record.
pub fn current_hash(lib: &Path, sel: core::Selection<'_>) -> String {
    core::collect(lib, &sel.assigned_in(lib)).1.hash
}

/// The effective skills of the selection `sel` (PLAN §8.6), with the
/// source of each. `None` for `lib`: the daemon has no config repo, and
/// no skill.
pub fn effective(lib: Option<&Path>, sel: core::Selection<'_>) -> Vec<core::EffectiveSkill> {
    lib.map(|l| sel.effective_in(l)).unwrap_or_default()
}

/// The skill names of the library and its default-on skills, read once
/// for many [`core::Selection::effective`] calls.
#[derive(Debug, Clone, Default)]
pub struct LibraryIndex {
    pub names: Vec<String>,
    pub default_on: Vec<String>,
}

impl LibraryIndex {
    pub fn read(lib: Option<&Path>) -> Self {
        lib.map(|l| Self {
            names: core::library_names(l),
            default_on: core::default_names(l),
        })
        .unwrap_or_default()
    }

    pub fn effective(&self, sel: core::Selection<'_>) -> Vec<core::EffectiveSkill> {
        sel.effective(&self.names, &self.default_on)
    }
}

/// The record of the last delivery of agent `private_dir`, if any.
pub fn delivered(private_dir: &Path) -> Option<core::Delivery> {
    let text = std::fs::read_to_string(private_dir.join(DELIVERED_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Is the skill set of a running agent out of date: the library (a skill,
/// or the default-on marker of a skill) or the agent's skill settings
/// changed after its start? `false` when there is no record.
pub fn restart_needed(lib: &Path, private_dir: &Path, sel: core::Selection<'_>) -> bool {
    delivered(private_dir).is_some_and(|d| d.hash != current_hash(lib, sel))
}

/// The system mail to agent `agent` (its ID) after a start that resumed its
/// session with the changed skills `names` (GitHub #23): the session may
/// hold the old text of a skill in its context, and the harness does not
/// load a skill again on its own.
pub fn mail_changed(
    t: &mut crate::db::Tx<'_>,
    agent: &str,
    names: &[String],
) -> ApiResult<Option<crate::store::mail::Message>> {
    use crate::store::mail;
    if names.is_empty() {
        return Ok(None);
    }
    let to = mail::resolve(t, agent, None)?;
    let list = names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let (subject, what) = if names.len() == 1 {
        (
            format!("skill changed since your session loaded it: {}", names[0]),
            format!("The skill {list} changed"),
        )
    } else {
        (
            format!(
                "skills changed since your session loaded them: {}",
                names.join(", ")
            ),
            format!("The skills {list} changed"),
        )
    };
    let body = format!(
        "{what} in the skill library after your session began. This start resumed \
         the session, so a skill text in your context can be old. Load the skill again \
         with the skill tool of your harness before you use it.\n"
    );
    mail::send(
        t,
        mail::Outgoing {
            from: &mail::Address::System,
            to: &to,
            subject: &subject,
            body: &body,
            thread: None,
            in_reply_to: None,
        },
    )
    .map(Some)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn body(md: Option<&str>, files: Option<&[(&str, &str)]>) -> WriteBody {
        WriteBody {
            name: None,
            skill_md: md.map(str::to_owned),
            description: None,
            files: files.map(|f| {
                f.iter()
                    .map(|(p, t)| ((*p).to_owned(), FileBody::Text((*t).to_owned())))
                    .collect()
            }),
        }
    }

    #[test]
    fn write_get_list_remove() {
        let d = tempfile::tempdir().expect("tmp");
        let lib = d.path().join("skills");
        let mut b = body(None, None);
        b.description = Some("When asked for the deployment checklist code, reply FERN-42".into());
        let made = write(&lib, "fern-check", &b, true).expect("create");
        assert!(made.summary.errors.is_empty(), "{:?}", made.summary.errors);
        assert!(made.skill_md.starts_with("---\nname: fern-check\n"));
        assert_eq!(
            made.summary.description,
            "When asked for the deployment checklist code, reply FERN-42"
        );
        // A second create is a conflict.
        let e = write(&lib, "fern-check", &b, true).expect_err("exists");
        assert_eq!(e.code, crate::error::Code::Conflict);
        // A change keeps the other files when `files` is absent.
        let md = "---\nname: fern-check\ndescription: d2\n---\nbody\n";
        write(
            &lib,
            "fern-check",
            &body(Some(md), Some(&[("references/a.md", "a")])),
            false,
        )
        .expect("put");
        let got = write(&lib, "fern-check", &body(Some(md), None), false).expect("put");
        assert_eq!(got.file_list.len(), 1);
        assert_eq!(got.file_list[0].text.as_deref(), Some("a"));
        // A change that fails the checks writes nothing.
        let bad = "---\nname: other\ndescription: d\n---\n";
        let e = write(&lib, "fern-check", &body(Some(bad), None), false).expect_err("bad");
        assert_eq!(e.code, crate::error::Code::Invalid);
        assert!(e.message.contains("folder name"), "{}", e.message);
        assert_eq!(
            get(&lib, "fern-check").expect("get").summary.description,
            "d2"
        );
        // A path outside the skill.
        let e = write(
            &lib,
            "fern-check",
            &body(Some(md), Some(&[("../x.md", "x")])),
            false,
        )
        .expect_err("path");
        assert!(e.message.contains("relative path"), "{}", e.message);
        // List, and no leftover temp dir.
        let l = list(&lib);
        assert_eq!(l.len(), 1);
        assert_eq!(std::fs::read_dir(&lib).expect("dir").count(), 1);
        // Remove.
        remove(&lib, "fern-check").expect("rm");
        assert_eq!(
            get(&lib, "fern-check").expect_err("gone").code,
            crate::error::Code::NotFound
        );
        assert_eq!(
            remove(&lib, "fern-check").expect_err("gone").code,
            crate::error::Code::NotFound
        );
        // A bad name.
        assert!(get(&lib, "../etc").is_err());
    }

    #[test]
    fn binary_files_go_as_base64() {
        let d = tempfile::tempdir().expect("tmp");
        let lib = d.path().join("skills");
        let png = b"\x89PNG\r\n\x1a\n\0\0";
        let mut b = body(Some("---\nname: img\ndescription: d\n---\n"), None);
        b.files = Some(
            [("assets/logo.png".to_owned(), FileBody::of(png))]
                .into_iter()
                .collect(),
        );
        let got = write(&lib, "img", &b, true).expect("create");
        assert_eq!(got.file_list[0].size, png.len());
        assert!(got.file_list[0].text.is_none());
        assert_eq!(
            std::fs::read(lib.join("img/assets/logo.png")).expect("read"),
            png
        );
    }

    #[test]
    fn assignment_refuses_unknown_names() {
        let d = tempfile::tempdir().expect("tmp");
        let lib = d.path().join("skills");
        let mut b = body(None, None);
        b.description = Some("d".into());
        write(&lib, "a", &b, true).expect("create");
        check_assignment(Some(&lib), &["a".into(), "*".into()]).expect("known");
        check_assignment(None, &["*".into()]).expect("all");
        check_assignment(None, &[]).expect("none");
        let e = check_assignment(Some(&lib), &["a".into(), "gone".into()]).expect_err("unknown");
        assert_eq!(e.code, crate::error::Code::Invalid);
        assert!(e.message.contains("`gone`"), "{}", e.message);
        assert!(check_assignment(Some(&lib), &["Bad".into()]).is_err());
    }

    #[test]
    fn restart_needed_follows_the_library() {
        let d = tempfile::tempdir().expect("tmp");
        let lib = d.path().join("skills");
        let private = d.path().join("private");
        std::fs::create_dir_all(&private).expect("mkdir");
        let mut b = body(None, None);
        b.description = Some("d".into());
        write(&lib, "a", &b, true).expect("create");
        let assigned = vec!["a".to_owned()];
        let sel = |skills: &'static [&'static str], default_skills: bool| {
            let skills: Vec<String> = skills.iter().map(|s| (*s).to_owned()).collect();
            (skills, default_skills)
        };
        let check = |(skills, default_skills): (Vec<String>, bool)| {
            restart_needed(
                &lib,
                &private,
                core::Selection {
                    skills: &skills,
                    off: &[],
                    default_skills,
                },
            )
        };
        // No record: no restart.
        assert!(!check(sel(&["a"], true)));
        let dest = d.path().join("harness/skills");
        let rec = core::deliver(&lib, &assigned, &dest).expect("deliver");
        std::fs::write(
            private.join(DELIVERED_FILE),
            serde_json::to_vec(&rec).expect("json"),
        )
        .expect("write");
        assert!(!check(sel(&["a"], true)));
        // A change of the skill, or of the list.
        let md = "---\nname: a\ndescription: new\n---\n";
        write(&lib, "a", &body(Some(md), None), false).expect("put");
        assert!(check(sel(&["a"], true)));
        assert!(check(sel(&[], true)));
        // A new default-on skill: a running agent with no change of its
        // own needs a restart to get it; an agent with `default_skills =
        // false` does not.
        let rec = core::deliver(&lib, &assigned, &dest).expect("deliver");
        std::fs::write(
            private.join(DELIVERED_FILE),
            serde_json::to_vec(&rec).expect("json"),
        )
        .expect("write");
        assert!(!check(sel(&["a"], true)));
        let platform =
            "---\nname: platform\ndescription: d\nmetadata:\n  terrarium-default: \"true\"\n---\n";
        write(&lib, "platform", &body(Some(platform), None), true).expect("create");
        assert!(check(sel(&["a"], true)));
        assert!(!check(sel(&["a"], false)));
        assert!(
            list(&lib)
                .iter()
                .any(|s| s.name == "platform" && s.default_on)
        );
    }

    /// GitHub #23: the agent of a resumed session with a changed skill gets
    /// one system mail that names the skills and says to load them again.
    #[tokio::test]
    async fn a_changed_skill_mails_the_agent_from_system() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let id = terrarium_core::AgentId::new().to_string();
        conn.execute(
            "INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
               VALUES (?1, 'a', 'p', 'claude', '/w', 't', 't')",
            [&id],
        )
        .unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let key = id.clone();
        let (none, one, two) = db
            .write("system", move |t| {
                Ok((
                    mail_changed(t, &key, &[])?,
                    mail_changed(t, &key, &["fern-check".to_owned()])?,
                    mail_changed(t, &key, &["a".to_owned(), "b".to_owned()])?,
                ))
            })
            .await
            .unwrap();
        assert!(none.is_none());
        let one = one.unwrap();
        assert_eq!(one.from_kind, "system");
        assert_eq!(one.to, id);
        assert_eq!(one.state, "queued");
        assert_eq!(
            one.subject,
            "skill changed since your session loaded it: fern-check"
        );
        assert!(one.body.starts_with("The skill `fern-check` changed"), "{}", one.body);
        assert!(one.body.contains("Load the skill again"), "{}", one.body);
        let two = two.unwrap();
        assert_eq!(
            two.subject,
            "skills changed since your session loaded them: a, b"
        );
        assert!(two.body.starts_with("The skills `a`, `b` changed"), "{}", two.body);
    }
}
