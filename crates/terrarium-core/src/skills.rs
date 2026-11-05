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
//! A skill can be "default on": its front matter has
//! `metadata: {terrarium-default: "true"}` ([`DEFAULT_KEY`]). The packaged
//! skills are default on. An agent, a template, or a fleet entry names
//! its explicit skills in `skills` (`"*"` is every skill of the library),
//! the default-on skills that it does not want in `skills_off`, and
//! `default_skills = false` turns off all default-on skills. The
//! effective set ([`effective`]) is (default-on skills, when
//! `default_skills`) + (explicit skills) - (`skills_off`). At each start
//! the daemon copies only the effective skills into
//! `<harness dir>/skills/` ([`deliver`]), which the sandbox binds
//! read-only, and each adapter points its harness there.
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
/// The key in the `metadata` map of the front matter that makes a skill
/// default on: `metadata:` then `  terrarium-default: "true"`.
pub const DEFAULT_KEY: &str = "terrarium-default";
/// The suffix of a new packaged default next to the operator's file
/// (PLAN §8.3). The skill reader skips such a file.
pub const NEW_DEFAULT_SUFFIX: &str = ".new";
/// The longest skill name.
pub const NAME_MAX: usize = 64;
/// The longest description.
pub const DESCRIPTION_MAX: usize = 1024;
/// The default of the most bytes in one skill, all files together
/// ([`max_bytes`]).
pub const SKILL_MAX_BYTES: usize = 8 * 1024 * 1024;
/// The default of the most files in one skill, `SKILL.md` included
/// ([`max_files`]).
pub const SKILL_MAX_FILES: usize = 256;
/// The env var that sets [`max_files`].
pub const MAX_FILES_ENV: &str = "TERRARIUM_SKILL_MAX_FILES";
/// The env var that sets [`max_bytes`]: a number of bytes, or a number
/// with `K`, `KiB`, `M`, or `MiB`.
pub const MAX_BYTES_ENV: &str = "TERRARIUM_SKILL_MAX_BYTES";
/// The most directory levels of a file path in a skill.
pub const PATH_MAX_DEPTH: usize = 4;
/// The file types that a skill may hold. The config repo `.gitignore`
/// admits the same types under `skills/`.
pub const EXTENSIONS: &[&str] = &[
    "md", "txt", "json", "yaml", "yml", "toml", "py", "sh", "js", "ts", "jsx", "tsx", "css",
    "html", "ps1", "psm1", "sql", "png", "jpg", "jpeg", "gif", "svg", "webp",
];
/// The types of [`EXTENSIONS`] that may hold binary data. A file of any
/// other type must be UTF-8 text with no NUL byte.
pub const BINARY_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
/// The file of an import dir that lists the files to skip, in gitignore
/// syntax ([`read_import_dir`]).
pub const IGNORE_FILE: &str = ".skillignore";

/// The most files in one skill: [`MAX_FILES_ENV`], else
/// [`SKILL_MAX_FILES`]. A value that does not parse, or `0`, is the
/// default.
pub fn max_files() -> usize {
    std::env::var(MAX_FILES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(SKILL_MAX_FILES)
}

/// The most bytes in one skill: [`MAX_BYTES_ENV`], else
/// [`SKILL_MAX_BYTES`]. A value that does not parse, or `0`, is the
/// default.
pub fn max_bytes() -> usize {
    std::env::var(MAX_BYTES_ENV)
        .ok()
        .and_then(|v| parse_size(&v))
        .filter(|n| *n > 0)
        .unwrap_or(SKILL_MAX_BYTES)
}

/// `8388608`, `8192K`, `8KiB`, `8M`, `8MiB` (any case) as bytes.
pub fn parse_size(v: &str) -> Option<usize> {
    let v = v.trim();
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (n, unit) = v.split_at(split);
    let n: usize = n.parse().ok()?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// The files of one skill: the path in the skill dir, and the bytes.
pub type Files = BTreeMap<String, Vec<u8>>;

/// The front matter of a `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub description: String,
    /// The skill is default on: `metadata.terrarium-default` is `true`
    /// ([`DEFAULT_KEY`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default_on: bool,
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

/// Is `bytes` a valid content for the file `rel` (a valid path)? A file
/// of a type of [`BINARY_EXTENSIONS`] may hold any bytes; a file of any
/// other type must be UTF-8 text with no NUL byte, so a text type does not
/// carry a binary.
pub fn check_content(rel: &str, bytes: &[u8]) -> Result<(), String> {
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if BINARY_EXTENSIONS.contains(&ext.as_str())
        || (!bytes.contains(&0) && std::str::from_utf8(bytes).is_ok())
    {
        Ok(())
    } else {
        Err(format!(
            "`{rel}`: a `.{ext}` file must be UTF-8 text; only {} may be binary",
            BINARY_EXTENSIONS.join(", ")
        ))
    }
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

/// Read `name`, `description`, and the default-on marker from the YAML
/// front matter of a `SKILL.md`. The top-level scalar keys are read: a
/// plain, quoted, or block (`|`, `>`) value. Of the `metadata` map, only
/// the scalar [`DEFAULT_KEY`] is read. Other keys (`license`, ...) may be
/// there and are not read.
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
    let mut metadata: BTreeMap<String, String> = BTreeMap::new();
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
        if k == "metadata" && v.is_empty() {
            // A map: one `key: value` on each indented line.
            while i < body.len() && (body[i].starts_with([' ', '\t']) || body[i].trim().is_empty())
            {
                if let Some((mk, mv)) = body[i].split_once(':') {
                    metadata.insert(mk.trim().to_owned(), unquote(mv));
                }
                i += 1;
            }
            continue;
        }
        if k == "metadata" && v.starts_with('{') && v.ends_with('}') {
            // A flow map: `{terrarium-default: "true"}`.
            for part in v[1..v.len() - 1].split(',') {
                if let Some((mk, mv)) = part.split_once(':') {
                    metadata.insert(unquote(mk), unquote(mv));
                }
            }
            continue;
        }
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
    let default_on = metadata
        .get(DEFAULT_KEY)
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes"));
    Ok(Meta {
        name: name.trim().to_owned(),
        description: description.trim().to_owned(),
        default_on,
    })
}

/// Check one skill: the name, `SKILL.md` and its front matter (the name
/// in it is the folder name, the description has at most
/// [`DESCRIPTION_MAX`] characters), each path ([`check_path`]), the caps
/// ([`max_files`], [`max_bytes`]), the content of each file
/// ([`check_content`]), and the secret scan of each file. The error is the
/// list of every problem.
pub fn check(name: &str, files: &Files) -> Result<Meta, Vec<String>> {
    let mut problems = Vec::new();
    if let Err(e) = check_name(name) {
        problems.push(e);
    }
    let (max_files, max_bytes) = (max_files(), max_bytes());
    if files.len() > max_files {
        problems.push(format!(
            "the skill has {} files; the limit is {max_files} ({MAX_FILES_ENV})",
            files.len()
        ));
    }
    let total: usize = files.values().map(Vec::len).sum();
    if total > max_bytes {
        problems.push(format!(
            "the skill has {total} bytes; the limit is {max_bytes} ({MAX_BYTES_ENV})"
        ));
    }
    for (path, bytes) in files {
        if let Err(e) = check_path(path) {
            problems.push(e);
        } else if let Err(e) = check_content(path, bytes) {
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
    walk_dir(dir, None).map(|i| i.files)
}

/// A skill dir read for an import ([`read_import_dir`]): the files, and
/// the paths that the import skipped, sorted (a skipped dir ends in `/`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Import {
    pub files: Files,
    pub skipped: Vec<String>,
}

/// Read a skill dir for an import (`terrarium skill new --from-dir`,
/// `terrarium skill import`): as [`read_dir`], but skip what is not part
/// of the skill instead of refusing it. Skipped: a hidden file or dir
/// (`.git/`, `.skillignore`, ...), `__pycache__/`, `*.pyc`, `*.pyo`, and
/// each path that [`IGNORE_FILE`] in `dir` matches (gitignore syntax).
/// The caps count only the files that are read.
pub fn read_import_dir(dir: &Path) -> Result<Import, Vec<String>> {
    let rules = match std::fs::read_to_string(dir.join(IGNORE_FILE)) {
        Ok(t) => IgnoreRules::parse(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => IgnoreRules::default(),
        Err(e) => return Err(vec![format!("cannot read {IGNORE_FILE}: {e}")]),
    };
    walk_dir(dir, Some(&rules))
}

/// Should an import skip `rel` (a path in the skill, `/` between parts)?
fn import_skips(rules: &IgnoreRules, rel: &str, is_dir: bool) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.starts_with('.')
        || (is_dir && name == "__pycache__")
        || (!is_dir && (name.ends_with(".pyc") || name.ends_with(".pyo")))
        || rules.ignored(rel, is_dir)
}

fn walk_dir(dir: &Path, import: Option<&IgnoreRules>) -> Result<Import, Vec<String>> {
    struct Walk<'a> {
        root: &'a Path,
        import: Option<&'a IgnoreRules>,
        out: Import,
        total: usize,
        max_files: usize,
        max_bytes: usize,
        problems: Vec<String>,
    }
    fn walk(w: &mut Walk<'_>, d: &Path) {
        let entries = match std::fs::read_dir(d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                w.problems.push(format!("cannot read {}: {e}", d.display()));
                return;
            }
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            let rel = path
                .strip_prefix(w.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    w.problems.push(format!("cannot read {rel}: {e}"));
                    continue;
                }
            };
            let ft = meta.file_type();
            // A new packaged default that waits for the operator
            // (`SKILL.md.new`, PLAN §8.3) is not a file of the skill.
            if ft.is_file() && rel.ends_with(NEW_DEFAULT_SUFFIX) {
                continue;
            }
            if let Some(rules) = w.import
                && import_skips(rules, &rel, ft.is_dir())
            {
                w.out
                    .skipped
                    .push(if ft.is_dir() { format!("{rel}/") } else { rel });
                continue;
            }
            if ft.is_symlink() {
                w.problems
                    .push(format!("`{rel}` is a symbolic link; a skill holds none"));
            } else if ft.is_dir() {
                walk(w, &path);
            } else if !ft.is_file() {
                w.problems.push(format!("`{rel}` is not a regular file"));
            } else if w.out.files.len() >= w.max_files {
                w.problems.push(format!(
                    "the skill has more than {} files ({MAX_FILES_ENV})",
                    w.max_files
                ));
                return;
            } else if w.total + meta.len() as usize > w.max_bytes {
                w.problems.push(format!(
                    "the skill has more than {} bytes ({MAX_BYTES_ENV})",
                    w.max_bytes
                ));
                return;
            } else {
                match std::fs::read(&path) {
                    Ok(b) => {
                        w.total += b.len();
                        w.out.files.insert(rel, b);
                    }
                    Err(e) => w.problems.push(format!("cannot read {rel}: {e}")),
                }
            }
        }
    }
    if let Ok(m) = std::fs::symlink_metadata(dir)
        && m.file_type().is_symlink()
    {
        return Err(vec![format!(
            "{} is a symbolic link; a skill holds none",
            dir.display()
        )]);
    }
    let mut w = Walk {
        root: dir,
        import,
        out: Import::default(),
        total: 0,
        max_files: max_files(),
        max_bytes: max_bytes(),
        problems: Vec::new(),
    };
    walk(&mut w, dir);
    if w.problems.is_empty() {
        w.out.skipped.sort();
        Ok(w.out)
    } else {
        Err(w.problems)
    }
}

/// The patterns of an [`IGNORE_FILE`], in gitignore syntax: one pattern
/// on each line; `#` starts a comment; `!` makes a pattern an exception;
/// a `/` at the end matches only a dir; a `/` at the start or in the
/// middle anchors the pattern at the skill dir, else it matches a name at
/// any level; `*` and `?` do not match `/`; `**` matches any number of
/// dirs; `[...]` is a class. The last pattern that matches wins. A file in
/// an ignored dir is ignored (the walk does not go into the dir).
#[derive(Debug, Clone, Default)]
pub struct IgnoreRules {
    rules: Vec<IgnoreRule>,
}

#[derive(Debug, Clone)]
struct IgnoreRule {
    re: regex::Regex,
    negate: bool,
    dir_only: bool,
}

impl IgnoreRules {
    /// Parse the text of an [`IGNORE_FILE`]. A pattern that does not
    /// compile is left out.
    pub fn parse(text: &str) -> Self {
        let rules = text
            .lines()
            .filter_map(|l| {
                let l = l.trim_end_matches(['\r', ' ', '\t']);
                if l.is_empty() || l.starts_with('#') {
                    return None;
                }
                let (negate, l) = match l.strip_prefix('!') {
                    Some(r) => (true, r),
                    None => (false, l.strip_prefix('\\').unwrap_or(l)),
                };
                let (dir_only, l) = match l.strip_suffix('/') {
                    Some(r) => (true, r),
                    None => (false, l),
                };
                let anchored = l.contains('/');
                let l = l.trim_start_matches('/');
                if l.is_empty() {
                    return None;
                }
                let body = glob_regex(l);
                let re = if anchored {
                    format!("^{body}$")
                } else {
                    format!("^(?:.*/)?{body}$")
                };
                regex::Regex::new(&re).ok().map(|re| IgnoreRule {
                    re,
                    negate,
                    dir_only,
                })
            })
            .collect();
        Self { rules }
    }

    /// Does a pattern ignore `rel` (a path in the skill dir, `/` between
    /// parts)?
    pub fn ignored(&self, rel: &str, is_dir: bool) -> bool {
        let mut out = false;
        for r in &self.rules {
            if (!r.dir_only || is_dir) && r.re.is_match(rel) {
                out = !r.negate;
            }
        }
        out
    }
}

/// The regex (not anchored) of a gitignore glob.
fn glob_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                let at_start = i == 0 || chars[i - 1] == '/';
                let at_end = i + 2 == chars.len();
                let slash_after = chars.get(i + 2) == Some(&'/');
                if at_start && slash_after {
                    // `**/`: zero or more dirs.
                    out.push_str("(?:.*/)?");
                    i += 3;
                    continue;
                }
                if at_start && at_end {
                    out.push_str(".*");
                } else {
                    out.push_str("[^/]*");
                }
                i += 2;
                continue;
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '[' => {
                if let Some(close) = chars[i + 1..].iter().position(|c| *c == ']') {
                    let class: String = chars[i + 1..i + 1 + close].iter().collect();
                    let class = class
                        .strip_prefix('!')
                        .map_or_else(|| class.clone(), |r| format!("^{r}"));
                    out.push('[');
                    out.push_str(&class.replace('\\', "\\\\").replace('[', "\\["));
                    out.push(']');
                    i += close + 2;
                    continue;
                }
                out.push_str("\\[");
            }
            '\\' if i + 1 < chars.len() => {
                out.push_str(&regex::escape(&chars[i + 1].to_string()));
                i += 2;
                continue;
            }
            c => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out
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

/// The default-on skills of the library dir `library`, sorted: each skill
/// whose `SKILL.md` front matter parses and has the marker
/// ([`DEFAULT_KEY`]). The full checks run at delivery.
pub fn default_names(library: &Path) -> Vec<String> {
    library_names(library)
        .into_iter()
        .filter(|n| {
            std::fs::read_to_string(library.join(n).join(SKILL_FILE))
                .ok()
                .and_then(|t| parse_front_matter(&t).ok())
                .is_some_and(|m| m.default_on)
        })
        .collect()
}

/// Check a `skills_off` list and give it as it is stored: as
/// [`clean_list`], but `*` is not valid (`default_skills = false` turns
/// off every default-on skill). A name that the library does not have is
/// valid: it has no effect.
pub fn clean_off_list(list: &[String]) -> Result<Vec<String>, String> {
    let out = clean_list(list)?;
    if out.iter().any(|n| n == ALL) {
        return Err(format!(
            "`{ALL}` is not valid in `skills_off`; to turn off every default skill, set \
             `default_skills` to false"
        ));
    }
    Ok(out)
}

/// Why an agent has a skill: the skill is default on, or the agent's
/// `skills` list names it (or has `*`). A skill that is both is
/// `explicit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Default,
    Explicit,
}

/// One skill of the effective set of an agent or a template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveSkill {
    pub name: String,
    pub source: Source,
}

/// The skill settings of an agent, a template, or a fleet entry: the
/// explicit `skills`, `skills_off`, and `default_skills`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection<'a> {
    pub skills: &'a [String],
    pub off: &'a [String],
    pub default_skills: bool,
}

impl Selection<'_> {
    /// The effective set, sorted by name: (the default-on skills
    /// `default_on`, when `default_skills`) + (the explicit skills that
    /// the library names `library` has; `*` is all of them) - `off`.
    pub fn effective(&self, library: &[String], default_on: &[String]) -> Vec<EffectiveSkill> {
        let off = |n: &String| self.off.contains(n);
        let mut out: BTreeMap<String, Source> = BTreeMap::new();
        if self.default_skills {
            for n in default_on.iter().filter(|n| library.contains(n) && !off(n)) {
                out.insert(n.clone(), Source::Default);
            }
        }
        for n in &resolve(self.skills, library).wanted {
            if !off(n) {
                out.insert(n.clone(), Source::Explicit);
            }
        }
        out.into_iter()
            .map(|(name, source)| EffectiveSkill { name, source })
            .collect()
    }

    /// The names that a delivery copies: the effective set, and each
    /// explicit name that the library does not have (the delivery
    /// reports it as missing).
    pub fn assigned(&self, library: &[String], default_on: &[String]) -> Vec<String> {
        let mut out: Vec<String> = self
            .effective(library, default_on)
            .into_iter()
            .map(|e| e.name)
            .collect();
        out.extend(
            resolve(self.skills, library)
                .missing
                .into_iter()
                .filter(|n| !self.off.contains(n)),
        );
        out
    }

    /// [`Self::effective`] for the library dir `library`.
    pub fn effective_in(&self, library: &Path) -> Vec<EffectiveSkill> {
        self.effective(&library_names(library), &default_names(library))
    }

    /// [`Self::assigned`] for the library dir `library`.
    pub fn assigned_in(&self, library: &Path) -> Vec<String> {
        self.assigned(&library_names(library), &default_names(library))
    }
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
    /// [`hash`] of each delivered skill alone, by name. Empty in a record
    /// from before this field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hashes: BTreeMap<String, String>,
}

impl Delivery {
    /// The skills of `self` that `earlier` delivered too, with other files
    /// (sorted). An added or removed skill does not count. Empty when
    /// `earlier` has no per-skill hashes (an older record).
    pub fn changed_since(&self, earlier: &Delivery) -> Vec<String> {
        self.hashes
            .iter()
            .filter(|(name, h)| earlier.hashes.get(*name).is_some_and(|old| old != *h))
            .map(|(name, _)| name.clone())
            .collect()
    }

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
    d.hashes = ok
        .iter()
        .map(|(n, f)| (n.clone(), hash([(n.as_str(), f)])))
        .collect();
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
    fn the_default_marker_is_in_metadata() {
        let block = "---\nname: a\ndescription: d\nmetadata:\n  terrarium-default: \"true\"\n  other: x\n---\n";
        assert!(parse_front_matter(block).expect("block").default_on);
        let flow = "---\nname: a\ndescription: d\nmetadata: {terrarium-default: true}\n---\n";
        assert!(parse_front_matter(flow).expect("flow").default_on);
        let off = "---\nname: a\ndescription: d\nmetadata:\n  terrarium-default: \"false\"\n---\n";
        assert!(!parse_front_matter(off).expect("off").default_on);
        // Only in `metadata`: a top-level key is not the marker.
        let top = "---\nname: a\ndescription: d\nterrarium-default: true\n---\n";
        assert!(!parse_front_matter(top).expect("top").default_on);
        // A key after the map is read again.
        let after = "---\nname: a\nmetadata:\n  terrarium-default: yes\ndescription: d\n---\n";
        let m = parse_front_matter(after).expect("after");
        assert!(m.default_on);
        assert_eq!(m.description, "d");
    }

    fn sel<'a>(skills: &'a [String], off: &'a [String], default_skills: bool) -> Selection<'a> {
        Selection {
            skills,
            off,
            default_skills,
        }
    }

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn the_effective_set_is_defaults_and_explicit_minus_off() {
        let lib = v(&["a", "b", "db", "tests"]);
        let defaults = v(&["db", "tests", "gone-default"]);
        let names = |e: Vec<EffectiveSkill>| {
            e.into_iter()
                .map(|e| format!("{}:{:?}", e.name, e.source))
                .collect::<Vec<_>>()
        };
        // No settings: the default-on skills of the library.
        assert_eq!(
            names(sel(&[], &[], true).effective(&lib, &defaults)),
            ["db:Default", "tests:Default"]
        );
        // Explicit skills come in; a default that is also explicit is explicit.
        assert_eq!(
            names(sel(&v(&["a", "db"]), &[], true).effective(&lib, &defaults)),
            ["a:Explicit", "db:Explicit", "tests:Default"]
        );
        // `skills_off` takes out a default and an explicit skill.
        assert_eq!(
            names(sel(&v(&["a", "b"]), &v(&["tests", "b"]), true).effective(&lib, &defaults)),
            ["a:Explicit", "db:Default"]
        );
        // `default_skills = false`: only the explicit skills.
        assert_eq!(
            names(sel(&v(&["a"]), &[], false).effective(&lib, &defaults)),
            ["a:Explicit"]
        );
        assert!(sel(&[], &[], false).effective(&lib, &defaults).is_empty());
        // `*` with an off list.
        assert_eq!(
            names(sel(&v(&["*"]), &v(&["b"]), false).effective(&lib, &defaults)),
            ["a:Explicit", "db:Explicit", "tests:Explicit"]
        );
        // The delivery list keeps a missing explicit name, for the warning.
        assert_eq!(
            sel(&v(&["a", "nope"]), &[], true).assigned(&lib, &defaults),
            ["a", "db", "tests", "nope"]
        );
        assert!(clean_off_list(&v(&["*"])).is_err());
        assert_eq!(clean_off_list(&v(&[" a ", "a"])).expect("ok"), ["a"]);
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
        for i in 0..max_files() {
            many.insert(format!("r/{i}.txt"), b"x".to_vec());
        }
        let e = check("fern", &many).expect_err("count");
        assert!(e.iter().any(|p| p.contains("files")), "{e:?}");
        let big = files(&[
            (SKILL_FILE, &md("fern", "d")),
            ("big.txt", &vec![b'a'; max_bytes()]),
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
    fn the_new_text_types_pass_and_a_text_type_holds_no_binary() {
        let mut f = files(&[(SKILL_FILE, &md("kit", "d"))]);
        for p in [
            "kit/button.jsx",
            "kit/card.tsx",
            "kit/theme.css",
            "pages/index.html",
            "win/setup.ps1",
            "win/Helpers.psm1",
            "db/schema.sql",
        ] {
            f.insert(p.to_owned(), b"text\n".to_vec());
        }
        check("kit", &f).expect("valid");
        // A binary in a text type is refused; an image may be binary.
        assert!(check_content("scripts/run.sh", b"\x7fELF\0\0").is_err());
        assert!(check_content("notes.txt", &[0xff, 0xfe, b'a']).is_err());
        assert!(check_content("assets/a.png", b"\x89PNG\0").is_ok());
        assert!(check_content("assets/a.svg", b"<svg/>").is_ok());
        let bad = files(&[(SKILL_FILE, &md("kit", "d")), ("tool.sh", b"\0\0")]);
        let e = check("kit", &bad).expect_err("binary");
        assert!(e.iter().any(|p| p.contains("UTF-8 text")), "{e:?}");
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("8388608"), Some(8 * 1024 * 1024));
        assert_eq!(parse_size("8M"), Some(8 * 1024 * 1024));
        assert_eq!(parse_size(" 16MiB "), Some(16 * 1024 * 1024));
        assert_eq!(parse_size("512k"), Some(512 * 1024));
        assert_eq!(parse_size("8G"), None);
        assert_eq!(parse_size("M"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn ignore_rules_follow_gitignore() {
        let r = IgnoreRules::parse(
            "# comment\n\n*.log\n/build/\ndocs/**/draft-*.md\n!keep.log\nsnap?.txt\n\
             [ab].tmp.md\nnode_modules/\n",
        );
        assert!(r.ignored("x.log", false));
        assert!(r.ignored("a/b/x.log", false));
        assert!(!r.ignored("keep.log", false), "a later `!` wins");
        assert!(!r.ignored("a/keep.log", false));
        assert!(r.ignored("build", true));
        assert!(!r.ignored("build", false), "dir only");
        assert!(!r.ignored("src/build", true), "anchored");
        assert!(r.ignored("docs/draft-1.md", false));
        assert!(r.ignored("docs/a/b/draft-1.md", false));
        assert!(!r.ignored("other/draft-1.md", false));
        assert!(r.ignored("snap1.txt", false));
        assert!(!r.ignored("snap12.txt", false));
        assert!(r.ignored("a.tmp.md", false));
        assert!(!r.ignored("c.tmp.md", false));
        assert!(r.ignored("web/node_modules", true));
        assert!(!r.ignored("SKILL.md", false));
        assert!(!IgnoreRules::default().ignored("x.log", false));
    }

    #[test]
    fn an_import_skips_caches_hidden_paths_and_ignored_files() {
        let d = tempfile::tempdir().expect("tmp");
        let s = d.path().join("kit");
        for dir in ["scripts/__pycache__", ".playwright-cli", "out", "docs"] {
            std::fs::create_dir_all(s.join(dir)).expect("mkdir");
        }
        std::fs::write(s.join(SKILL_FILE), md("kit", "d")).expect("write");
        std::fs::write(s.join("scripts/run.py"), b"print(1)\n").expect("write");
        std::fs::write(s.join("scripts/__pycache__/run.cpython-312.pyc"), b"\0").expect("w");
        std::fs::write(s.join("scripts/old.pyc"), b"\0").expect("write");
        std::fs::write(s.join(".playwright-cli/console.log"), b"x").expect("write");
        std::fs::write(s.join(".DS_Store"), b"\0").expect("write");
        std::fs::write(s.join("out/page.yml"), b"x").expect("write");
        std::fs::write(s.join("docs/a.md"), b"a").expect("write");
        std::fs::write(s.join("docs/draft.md"), b"d").expect("write");
        std::fs::write(s.join(IGNORE_FILE), b"out/\ndraft.md\n").expect("write");

        let got = read_import_dir(&s).expect("import");
        assert_eq!(
            got.files.keys().collect::<Vec<_>>(),
            [SKILL_FILE, "docs/a.md", "scripts/run.py"]
        );
        assert_eq!(
            got.skipped,
            [
                ".DS_Store",
                ".playwright-cli/",
                ".skillignore",
                "docs/draft.md",
                "out/",
                "scripts/__pycache__/",
                "scripts/old.pyc",
            ]
        );
        check("kit", &got.files).expect("valid after the skips");
        // The library read skips nothing: the hidden dir is a problem for
        // the checks, and the cache is not a valid type.
        let lib = read_dir(&s).expect("read");
        assert!(check("kit", &lib).is_err());
        // A symbolic link is still refused.
        std::os::unix::fs::symlink("/etc/passwd", s.join("passwd.txt")).expect("link");
        assert!(read_import_dir(&s).is_err());
    }

    #[test]
    fn read_dir_refuses_symlinks() {
        let d = tempfile::tempdir().expect("tmp");
        let s = d.path().join("fern");
        std::fs::create_dir_all(s.join("scripts")).expect("mkdir");
        std::fs::write(s.join(SKILL_FILE), md("fern", "d")).expect("write");
        std::fs::write(s.join("scripts/a.sh"), b"x").expect("write");
        // A new packaged default that waits for the operator is not a file of the skill.
        std::fs::write(s.join("SKILL.md.new"), md("fern", "newer")).expect("write");
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

        // A default-on skill comes with no `skills` list.
        write_files(
            &lib.join("platform"),
            &files(&[(
                SKILL_FILE,
                b"---\nname: platform\ndescription: d\nmetadata:\n  terrarium-default: \"true\"\n---\n",
            )]),
        )
        .expect("write");
        assert_eq!(default_names(&lib), ["platform"]);
        let s = sel(&[], &[], true);
        let got_default = deliver(&lib, &s.assigned_in(&lib), &dest).expect("default");
        assert_eq!(got_default.names, ["platform"]);
        assert!(dest.join("platform/SKILL.md").is_file());
        let off = v(&["platform"]);
        let got_off = deliver(&lib, &sel(&[], &off, true).assigned_in(&lib), &dest).expect("off");
        assert!(got_off.names.is_empty());
        std::fs::remove_dir_all(lib.join("platform")).expect("rm");

        // The hash follows the content.
        let one = deliver(&lib, &["fern-check".into()], &dest).expect("one");
        assert_eq!(one.hash, got.hash);
        std::fs::write(lib.join("fern-check/references/r.md"), b"changed").expect("write");
        let (_, now) = collect(&lib, &["fern-check".into()]);
        assert_ne!(now.hash, one.hash);
    }

    /// GitHub #23: the skills that changed between two deliveries. Only a
    /// skill that both have counts; an older record gives none.
    #[test]
    fn changed_since_names_the_changed_skills() {
        let d = tempfile::tempdir().expect("tmp");
        let lib = d.path().join("lib");
        for n in ["fern-check", "other", "late"] {
            write_files(&lib.join(n), &files(&[(SKILL_FILE, &md(n, "d"))])).expect("write");
        }
        let both = ["fern-check".to_owned(), "other".to_owned()];
        let (_, before) = collect(&lib, &both);
        assert_eq!(before.hashes.len(), 2);
        let (_, same) = collect(&lib, &both);
        assert!(same.changed_since(&before).is_empty());

        // A changed file of one skill; another skill added, and one gone.
        std::fs::write(lib.join("fern-check/SKILL.md"), md("fern-check", "new step")).expect("write");
        let (_, after) = collect(&lib, &["fern-check".to_owned(), "late".to_owned()]);
        assert_eq!(after.changed_since(&before), ["fern-check"]);

        // An extra file counts as a change too.
        let (_, one) = collect(&lib, &["other".to_owned()]);
        std::fs::write(lib.join("other/extra.md"), b"x").expect("write");
        let (_, more) = collect(&lib, &["other".to_owned()]);
        assert_eq!(more.changed_since(&one), ["other"]);

        // A record from before the per-skill hashes: no change is known.
        let old: Delivery =
            serde_json::from_str(r#"{"names":["fern-check"],"hash":"x"}"#).expect("old");
        assert!(old.hashes.is_empty());
        assert!(after.changed_since(&old).is_empty());
    }
}
