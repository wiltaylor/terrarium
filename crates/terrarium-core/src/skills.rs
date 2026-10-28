//! Terrarium-managed skills (PLAN §8.6): the skill library in the config
//! repo, the checks of a skill, and the copy of an agent's skills at each
//! start.
//!
//! A skill is a folder in the open Agent Skills format:
//! `skills/<name>/SKILL.md` with YAML front matter (`name`,
//! `description`), then Markdown, and optional files (`scripts/`,
//! `references/`, `assets/`). The library is `<config repo>/skills/`.
//! Terrarium ships no skill.
//!
//! An agent, a template, or a fleet entry names its skills in `skills`
//! (default none; `"*"` is every skill of the library). At each start the
//! daemon copies only those skills into `<harness dir>/skills/`
//! ([`deliver`]), which the sandbox binds read-only, and each adapter
//! points its harness there.
//!
//! The checks ([`check`]) run on each write through the API and at each
//! delivery. A skill that fails them is not delivered.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The library dir in the config repo.
pub const LIBRARY_DIR: &str = "skills";
/// The dir of the delivered skills in the agent's harness dir.
pub const DELIVERED_DIR: &str = "skills";
/// The main file of a skill.
pub const SKILL_FILE: &str = "SKILL.md";
/// The name in a `skills` list that means "every skill of the library".
pub const ALL: &str = "*";
/// The longest skill name.
pub const NAME_MAX: usize = 64;
/// The longest description.
pub const DESCRIPTION_MAX: usize = 1024;
/// The most bytes in one skill, all files together.
pub const SKILL_MAX_BYTES: usize = 1024 * 1024;
/// The most files in one skill, `SKILL.md` included.
pub const SKILL_MAX_FILES: usize = 64;
/// The most directory levels of a file path in a skill.
pub const PATH_MAX_DEPTH: usize = 4;
/// The file types that a skill may hold. The config repo `.gitignore`
/// admits the same types under `skills/`.
pub const EXTENSIONS: &[&str] = &[
    "md", "txt", "json", "yaml", "yml", "toml", "py", "sh", "js", "ts", "png", "jpg", "jpeg",
    "gif", "svg", "webp",
];

/// The files of one skill: the path in the skill dir, and the bytes.
pub type Files = BTreeMap<String, Vec<u8>>;

/// The front matter of a `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub description: String,
}

/// Is `name` a valid skill name? Lowercase letters, digits, and single
/// `-` between them (kebab-case), at most [`NAME_MAX`] characters.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= NAME_MAX
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "the skill name `{name}` is not valid: use lowercase letters, digits, and `-` \
             (kebab-case), at most {NAME_MAX} characters"
        ))
    }
}

/// Check a `skills` list of an agent, a template, or a fleet entry, and
/// give it as it is stored: names trimmed, no empty name, no duplicate,
/// in the given order. `*` is valid.
pub fn clean_list(list: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for n in list.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if n != ALL {
            check_name(n)?;
        }
        if !out.iter().any(|o| o == n) {
            out.push(n.to_owned());
        }
    }
    Ok(out)
}

/// Is `rel` a valid file path in a skill? Relative, at most
/// [`PATH_MAX_DEPTH`] parts, no `..`, no hidden part, and a type of
/// [`EXTENSIONS`].
pub fn check_path(rel: &str) -> Result<(), String> {
    let p = Path::new(rel);
    let parts: Vec<&str> = rel.split('/').collect();
    let normal = !rel.is_empty()
        && !rel.contains('\\')
        && p.components().all(|c| matches!(c, Component::Normal(_)))
        && parts.iter().all(|s| !s.is_empty() && !s.starts_with('.'));
    if !normal {
        return Err(format!(
            "`{rel}`: use a relative path with no `..`, no hidden part, and no empty part"
        ));
    }
    if parts.len() > PATH_MAX_DEPTH {
        return Err(format!(
            "`{rel}`: at most {PATH_MAX_DEPTH} levels of directories"
        ));
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if !ext.as_deref().is_some_and(|e| EXTENSIONS.contains(&e)) {
        return Err(format!(
            "`{rel}`: the file type is not allowed; allowed: {}",
            EXTENSIONS.join(", ")
        ));
    }
    Ok(())
}

/// A YAML scalar as it is written after `key:`, without its quotes.
fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        let inner = &v[1..v.len() - 1];
        if v.starts_with('\'') {
            return inner.replace("''", "'");
        }
        return inner.replace("\\\"", "\"").replace("\\\\", "\\");
    }
    v.to_owned()
}

/// Read `name` and `description` from the YAML front matter of a
/// `SKILL.md`. Only the top-level scalar keys are read: a plain, quoted,
/// or block (`|`, `>`) value. Other keys (`license`, `metadata`, ...)
/// may be there and are not read.
pub fn parse_front_matter(text: &str) -> Result<Meta, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Err(format!(
            "{SKILL_FILE} must start with YAML front matter: a `---` line, `name:`, \
             `description:`, and a `---` line"
        ));
    }
    let mut body: Vec<&str> = Vec::new();
    let mut closed = false;
    for l in lines {
        if l.trim_end() == "---" {
            closed = true;
            break;
        }
        body.push(l);
    }
    if !closed {
        return Err(format!(
            "the front matter of {SKILL_FILE} has no closing `---` line"
        ));
    }
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        i += 1;
        if line.starts_with([' ', '\t']) || line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim().to_owned();
        let v = v.trim();
        if v.starts_with('|') || v.starts_with('>') {
            let mut parts = Vec::new();
            while i < body.len() && (body[i].starts_with([' ', '\t']) || body[i].trim().is_empty())
            {
                parts.push(body[i].trim());
                i += 1;
            }
            let joined = if v.starts_with('|') {
                parts.join("\n")
            } else {
                parts.join(" ")
            };
            keys.insert(k, joined.trim().to_owned());
        } else {
            // A plain scalar can go on in indented lines.
            let mut value = unquote(v);
            while i < body.len() && body[i].starts_with([' ', '\t']) && !body[i].trim().is_empty() {
                value.push(' ');
                value.push_str(body[i].trim());
                i += 1;
            }
            keys.insert(k, value);
        }
    }
    let name = keys.remove("name").unwrap_or_default();
    let description = keys.remove("description").unwrap_or_default();
    if name.trim().is_empty() {
        return Err(format!("the front matter of {SKILL_FILE} has no `name`"));
    }
    if description.trim().is_empty() {
        return Err(format!(
            "the front matter of {SKILL_FILE} has no `description`"
        ));
    }
    Ok(Meta {
        name: name.trim().to_owned(),
        description: description.trim().to_owned(),
    })
}

/// Check one skill: the name, `SKILL.md` and its front matter (the name
/// in it is the folder name, the description has at most
/// [`DESCRIPTION_MAX`] characters), each path ([`check_path`]), the caps
/// ([`SKILL_MAX_FILES`], [`SKILL_MAX_BYTES`]), and the secret scan of each
/// file. The error is the list of every problem.
pub fn check(name: &str, files: &Files) -> Result<Meta, Vec<String>> {
    let mut problems = Vec::new();
    if let Err(e) = check_name(name) {
        problems.push(e);
    }
    if files.len() > SKILL_MAX_FILES {
        problems.push(format!(
            "the skill has {} files; the limit is {SKILL_MAX_FILES}",
            files.len()
        ));
    }
    let total: usize = files.values().map(Vec::len).sum();
    if total > SKILL_MAX_BYTES {
        problems.push(format!(
            "the skill has {total} bytes; the limit is {SKILL_MAX_BYTES}"
        ));
    }
    for (path, bytes) in files {
        if let Err(e) = check_path(path) {
            problems.push(e);
        }
        for f in crate::secret_scan::scan(&format!("{LIBRARY_DIR}/{name}/{path}"), bytes) {
            problems.push(format!(
                "the secret scan: {f}. Remove the secret, or for a false hit add `{}` to the line",
                crate::secret_scan::ALLOW_MARKER
            ));
        }
    }
    let meta = match files.get(SKILL_FILE) {
        None => {
            problems.push(format!("the skill has no {SKILL_FILE}"));
            None
        }
        Some(bytes) => match std::str::from_utf8(bytes) {
            Err(_) => {
                problems.push(format!("{SKILL_FILE} is not UTF-8 text"));
                None
            }
            Ok(text) => match parse_front_matter(text) {
                Err(e) => {
                    problems.push(e);
                    None
                }
                Ok(m) => {
                    if m.name != name {
                        problems.push(format!(
                            "the front matter `name: {}` is not the folder name `{name}`",
                            m.name
                        ));
                    }
                    let n = m.description.chars().count();
                    if n > DESCRIPTION_MAX {
                        problems.push(format!(
                            "the description has {n} characters; the limit is {DESCRIPTION_MAX}"
                        ));
                    }
                    Some(m)
                }
            },
        },
    };
    match meta {
        Some(m) if problems.is_empty() => Ok(m),
        _ => Err(problems),
    }
}

/// Read the files of the skill dir `dir`. A symbolic link, a file that is
/// not a regular file, and a skill past the caps are errors. A missing
/// dir is an empty map.
pub fn read_dir(dir: &Path) -> Result<Files, Vec<String>> {
    fn walk(root: &Path, d: &Path, out: &mut Files, total: &mut usize, problems: &mut Vec<String>) {
        let entries = match std::fs::read_dir(d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                problems.push(format!("cannot read {}: {e}", d.display()));
                return;
            }
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    problems.push(format!("cannot read {rel}: {e}"));
                    continue;
                }
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                problems.push(format!("`{rel}` is a symbolic link; a skill holds none"));
            } else if ft.is_dir() {
                walk(root, &path, out, total, problems);
            } else if !ft.is_file() {
                problems.push(format!("`{rel}` is not a regular file"));
            } else if out.len() >= SKILL_MAX_FILES {
                problems.push(format!("the skill has more than {SKILL_MAX_FILES} files"));
                return;
            } else if *total + meta.len() as usize > SKILL_MAX_BYTES {
                problems.push(format!("the skill has more than {SKILL_MAX_BYTES} bytes"));
                return;
            } else {
                match std::fs::read(&path) {
                    Ok(b) => {
                        *total += b.len();
                        out.insert(rel, b);
                    }
                    Err(e) => problems.push(format!("cannot read {rel}: {e}")),
                }
            }
        }
    }
    let mut out = Files::new();
    let mut problems = Vec::new();
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(vec![format!(
                "{} is a symbolic link; a skill holds none",
                dir.display()
            )]);
        }
        _ => {}
    }
    let mut total = 0;
    walk(dir, dir, &mut out, &mut total, &mut problems);
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

/// The skill names in the library dir `library`, sorted: each sub dir
/// whose name is a valid skill name. A hidden dir and a file are not
/// skills.
pub fn library_names(library: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(library) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| check_name(n).is_ok())
        .collect();
    names.sort();
    names
}

/// The skills that a `skills` list picks from the library names `library`:
/// `wanted` (sorted, `*` is all of the library), and `missing` (the
/// names that the library does not have).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub wanted: Vec<String>,
    pub missing: Vec<String>,
}

pub fn resolve(assigned: &[String], library: &[String]) -> Resolved {
    let mut r = Resolved::default();
    for n in assigned {
        if n == ALL {
            r.wanted.extend(library.iter().cloned());
        } else if library.contains(n) {
            r.wanted.push(n.clone());
        } else {
            r.missing.push(n.clone());
        }
    }
    r.wanted.sort();
    r.wanted.dedup();
    r.missing.sort();
    r.missing.dedup();
    r
}

/// The hash of a set of skills (name, path, bytes; sorted): the
/// "restart to apply" check compares the hash of the delivered skills with
/// the hash of the library now. No skill: the empty string.
pub fn hash<'a>(skills: impl IntoIterator<Item = (&'a str, &'a Files)>) -> String {
    let mut h = Sha256::new();
    let mut any = false;
    let mut sorted: Vec<(&str, &Files)> = skills.into_iter().collect();
    sorted.sort_by_key(|(n, _)| *n);
    for (name, files) in sorted {
        any = true;
        h.update(name.as_bytes());
        h.update([0]);
        for (path, bytes) in files {
            h.update(path.as_bytes());
            h.update([0]);
            h.update((bytes.len() as u64).to_le_bytes());
            h.update(bytes);
        }
        h.update([1]);
    }
    if !any {
        return String::new();
    }
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// What a delivery copied, and why it left a skill out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// The delivered skills, sorted.
    pub names: Vec<String>,
    /// [`hash`] of the delivered skills.
    pub hash: String,
    /// The assigned names that the library does not have (deleted after
    /// the assignment).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// The skills that fail [`check`], with the first problem.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalid: Vec<String>,
}

impl Delivery {
    /// One warning line for each skill that is not delivered.
    pub fn warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .missing
            .iter()
            .map(|n| format!("the skill `{n}` is not in the skill library; it is not delivered"))
            .collect();
        out.extend(
            self.invalid
                .iter()
                .map(|p| format!("a skill is not delivered: {p}")),
        );
        out
    }
}

/// Read and check the skills that `assigned` picks from `library` (the
/// library dir), without a copy. Returns the valid skills with their
/// files, and the delivery record.
pub fn collect(library: &Path, assigned: &[String]) -> (Vec<(String, Files)>, Delivery) {
    let names = library_names(library);
    let r = resolve(assigned, &names);
    let mut ok = Vec::new();
    let mut d = Delivery {
        missing: r.missing,
        ..Delivery::default()
    };
    for name in r.wanted {
        let files = match read_dir(&library.join(&name)) {
            Ok(f) => f,
            Err(p) => {
                d.invalid.push(format!("`{name}`: {}", p.join("; ")));
                continue;
            }
        };
        match check(&name, &files) {
            Ok(_) => ok.push((name, files)),
            Err(p) => d.invalid.push(format!("`{name}`: {}", p.join("; "))),
        }
    }
    d.names = ok.iter().map(|(n, _)| n.clone()).collect();
    d.hash = hash(ok.iter().map(|(n, f)| (n.as_str(), f)));
    (ok, d)
}

/// Copy the skills that `assigned` picks from `library` into `dest`: the
/// dir is made again, empty, at each call, so a skill that the agent lost
/// goes. `dest` exists after the call, also with no skill (a harness bind
/// needs a source). The files get mode 0644, the dirs 0755: the sandbox
/// binds the dir read-only.
pub fn deliver(library: &Path, assigned: &[String], dest: &Path) -> Result<Delivery, String> {
    let (skills, d) = collect(library, assigned);
    match std::fs::symlink_metadata(dest) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => std::fs::remove_dir_all(dest)
            .map_err(|e| format!("cannot empty {}: {e}", dest.display()))?,
        Ok(_) => std::fs::remove_file(dest)
            .map_err(|e| format!("cannot remove {}: {e}", dest.display()))?,
        Err(_) => {}
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    for (name, files) in &skills {
        write_files(&dest.join(name), files)?;
    }
    Ok(d)
}

/// Write `files` under `dir` (made when missing).
pub fn write_files(dir: &Path, files: &Files) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for (rel, bytes) in files {
        check_path(rel)?;
        let path: PathBuf = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

/// The delivered skills dir of an agent whose harness dir is `harness_dir`.
pub fn delivered_dir(harness_dir: &Path) -> PathBuf {
    harness_dir.join(DELIVERED_DIR)
}

/// Copy the skills `names` of the delivered dir `skills_dir` into `dest`,
/// which is made again, empty (an adapter that needs its own layout, as
/// the Claude Code plugin dir).
pub fn copy_delivered(skills_dir: &Path, names: &[String], dest: &Path) -> Result<(), String> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .map_err(|e| format!("cannot empty {}: {e}", dest.display()))?;
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    for name in names {
        let files = read_dir(&skills_dir.join(name)).map_err(|p| p.join("; "))?;
        write_files(&dest.join(name), &files)?;
    }
    Ok(())
}

/// The front matter of each delivered skill `names` in `skills_dir`, in
/// order. A skill whose `SKILL.md` does not parse is left out (the
/// delivery checked it).
pub fn describe(skills_dir: &Path, names: &[String]) -> Vec<Meta> {
    names
        .iter()
        .filter_map(|n| {
            let text = std::fs::read_to_string(skills_dir.join(n).join(SKILL_FILE)).ok()?;
            parse_front_matter(&text).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(name: &str, description: &str) -> Vec<u8> {
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nDo it.\n")
            .into_bytes()
    }

    fn files(pairs: &[(&str, &[u8])]) -> Files {
        pairs
            .iter()
            .map(|(p, b)| ((*p).to_owned(), b.to_vec()))
            .collect()
    }

    #[test]
    fn names_are_kebab_case() {
        for ok in ["fern-check", "a", "a1-b2", &"x".repeat(NAME_MAX)] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Fern",
            "fern_check",
            "-a",
            "a-",
            "a--b",
            "a b",
            "../x",
            ".hidden",
            &"x".repeat(NAME_MAX + 1),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn lists_are_cleaned() {
        let l = clean_list(&[" a ".into(), "".into(), "*".into(), "a".into()]).expect("ok");
        assert_eq!(l, ["a", "*"]);
        assert!(clean_list(&["Bad".into()]).is_err());
    }

    #[test]
    fn front_matter_forms() {
        let m = parse_front_matter(
            "---\nname: fern-check\ndescription: \"Reply: FERN-42\"\nlicense: MIT\n---\nbody\n",
        )
        .expect("ok");
        assert_eq!(m.name, "fern-check");
        assert_eq!(m.description, "Reply: FERN-42");
        let m = parse_front_matter(
            "---\nname: 'a'\ndescription: >\n  Two\n  lines.\nmetadata:\n  k: v\n---\n",
        )
        .expect("block");
        assert_eq!(m.description, "Two lines.");
        let m = parse_front_matter("---\nname: a\ndescription: one\n  two\n---\n").expect("plain");
        assert_eq!(m.description, "one two");
        assert!(parse_front_matter("# no front matter").is_err());
        assert!(parse_front_matter("---\nname: a\n").is_err());
        assert!(parse_front_matter("---\nname: a\n---\n").is_err());
        assert!(parse_front_matter("---\ndescription: d\n---\n").is_err());
    }

    #[test]
    fn a_valid_skill_passes() {
        let f = files(&[
            (SKILL_FILE, &md("fern-check", "When asked, reply FERN-42.")),
            ("scripts/run.sh", b"echo hi\n"),
            ("references/notes.md", b"notes\n"),
            ("assets/logo.png", b"\x89PNG\0\0"),
        ]);
        let m = check("fern-check", &f).expect("valid");
        assert_eq!(m.description, "When asked, reply FERN-42.");
    }

    #[test]
    fn each_rule_is_checked() {
        // The name in the front matter is the folder name.
        let f = files(&[(SKILL_FILE, &md("other", "d"))]);
        let e = check("fern", &f).expect_err("name");
        assert!(e[0].contains("folder name"), "{e:?}");
        // No SKILL.md.
        let e = check("fern", &files(&[("a.md", b"x")])).expect_err("missing");
        assert!(e.iter().any(|p| p.contains("no SKILL.md")), "{e:?}");
        // The description cap.
        let long = "x".repeat(DESCRIPTION_MAX + 1);
        let e = check("fern", &files(&[(SKILL_FILE, &md("fern", &long))])).expect_err("long");
        assert!(e[0].contains("characters"), "{e:?}");
        // Paths and types.
        for bad in [
            "../x.md",
            ".hidden.md",
            "a/.b/c.md",
            "bin/tool",
            "x.exe",
            "a/b/c/d/e.md",
        ] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
        // The caps.
        let mut many = files(&[(SKILL_FILE, &md("fern", "d"))]);
        for i in 0..SKILL_MAX_FILES {
            many.insert(format!("r/{i}.txt"), b"x".to_vec());
        }
        let e = check("fern", &many).expect_err("count");
        assert!(e.iter().any(|p| p.contains("files")), "{e:?}");
        let big = files(&[
            (SKILL_FILE, &md("fern", "d")),
            ("big.txt", &vec![b'a'; SKILL_MAX_BYTES]),
        ]);
        let e = check("fern", &big).expect_err("bytes");
        assert!(e.iter().any(|p| p.contains("bytes")), "{e:?}");
        // The secret scan.
        let secret = files(&[
            (SKILL_FILE, &md("fern", "d")),
            (
                "scripts/x.sh",
                b"export TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789\n",
            ),
        ]);
        let e = check("fern", &secret).expect_err("secret");
        assert!(
            e.iter().any(|p| p.contains("skills/fern/scripts/x.sh:1")),
            "{e:?}"
        );
        assert!(!e.iter().any(|p| p.contains("ghp_")), "never the value");
    }

    #[test]
    fn read_dir_refuses_symlinks() {
        let d = tempfile::tempdir().expect("tmp");
        let s = d.path().join("fern");
        std::fs::create_dir_all(s.join("scripts")).expect("mkdir");
        std::fs::write(s.join(SKILL_FILE), md("fern", "d")).expect("write");
        std::fs::write(s.join("scripts/a.sh"), b"x").expect("write");
        let f = read_dir(&s).expect("read");
        assert_eq!(f.keys().collect::<Vec<_>>(), [SKILL_FILE, "scripts/a.sh"]);
        std::os::unix::fs::symlink("/etc/passwd", s.join("passwd.txt")).expect("link");
        let e = read_dir(&s).expect_err("link");
        assert!(e[0].contains("symbolic link"), "{e:?}");
    }

    #[test]
    fn resolve_takes_all_and_reports_missing() {
        let lib = vec!["a".to_owned(), "b".to_owned()];
        let r = resolve(&["*".into()], &lib);
        assert_eq!(r.wanted, ["a", "b"]);
        let r = resolve(&["b".into(), "gone".into()], &lib);
        assert_eq!(r.wanted, ["b"]);
        assert_eq!(r.missing, ["gone"]);
        assert_eq!(resolve(&[], &lib), Resolved::default());
    }

    #[test]
    fn deliver_copies_only_the_assigned_skills() {
        let d = tempfile::tempdir().expect("tmp");
        let lib = d.path().join("lib");
        for n in ["fern-check", "other"] {
            write_files(
                &lib.join(n),
                &files(&[(SKILL_FILE, &md(n, "d")), ("references/r.md", b"r")]),
            )
            .expect("write");
        }
        // A broken skill and a hidden dir.
        write_files(&lib.join("broken"), &files(&[("a.md", b"x")])).expect("write");
        std::fs::create_dir_all(lib.join(".git")).expect("mkdir");
        let dest = d.path().join("agent/skills");
        std::fs::create_dir_all(dest.join("stale")).expect("mkdir");

        let got = deliver(&lib, &["fern-check".into(), "gone".into()], &dest).expect("deliver");
        assert_eq!(got.names, ["fern-check"]);
        assert_eq!(got.missing, ["gone"]);
        assert!(dest.join("fern-check/SKILL.md").is_file());
        assert!(dest.join("fern-check/references/r.md").is_file());
        assert!(!dest.join("other").exists());
        assert!(!dest.join("stale").exists(), "a fresh dir each start");
        assert_eq!(got.warnings().len(), 1);

        let all = deliver(&lib, &["*".into()], &dest).expect("all");
        assert_eq!(all.names, ["fern-check", "other"]);
        assert_eq!(all.invalid.len(), 1, "{:?}", all.invalid);
        assert!(all.invalid[0].starts_with("`broken`"));

        // No skill: the dir exists and is empty; the hash is empty.
        let none = deliver(&lib, &[], &dest).expect("none");
        assert!(none.names.is_empty() && none.hash.is_empty());
        assert_eq!(std::fs::read_dir(&dest).expect("dir").count(), 0);

        // The hash follows the content.
        let one = deliver(&lib, &["fern-check".into()], &dest).expect("one");
        assert_eq!(one.hash, got.hash);
        std::fs::write(lib.join("fern-check/references/r.md"), b"changed").expect("write");
        let (_, now) = collect(&lib, &["fern-check".into()]);
        assert_ne!(now.hash, one.hash);
    }
}
