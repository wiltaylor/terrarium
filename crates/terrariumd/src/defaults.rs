//! The packaged defaults and their copy in the config repo (PLAN §8.3).
//!
//! The image ships the default prompts and harness settings in
//! `/opt/terrarium/defaults/` (`TERRARIUM_DEFAULTS_DIR`; the repo dir
//! `defaults/`). `manifest.json` lists each file with the SHA-256 of its
//! content. At each start the daemon copies them into the config repo
//! (`TERRARIUM_CONFIG_REPO`, `/config/terrarium`), at the same relative
//! path, with these rules ([`seed`]):
//!
//! | The file in the config repo | What the start does |
//! |---|---|
//! | missing | writes the default (a template under `templates/` that an earlier start wrote: nothing, the operator deleted it) |
//! | the same as the new default | nothing |
//! | the same as the default that the last start wrote (not edited) | writes the new default |
//! | edited | keeps the file, and writes the new default next to it as `<file>.new` |
//! | edited, and the operator kept it against this default ([`keep`]) | nothing |
//!
//! `.terrarium-defaults.json` in the config repo records the default that
//! each file came from. It is per instance: the repo's `.gitignore` keeps
//! it, and the `.new` files, out of git. The dashboard and
//! `terrarium admin config defaults` show the `.new` files with a diff
//! ([`pending`]); [`accept`] takes the new default, [`keep`] keeps the
//! edit. [`apply`] (`just defaults::apply`) backs the config repo up and
//! writes every default over it.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file list of the packaged defaults.
pub const MANIFEST: &str = "manifest.json";
/// The record of the seeded defaults in the config repo.
pub const RECORD: &str = ".terrarium-defaults.json";
/// The suffix of a new default that waits for the operator.
pub const NEW_SUFFIX: &str = ".new";
/// The default of `TERRARIUM_DEFAULTS_DIR`.
pub const DEFAULT_DEFAULTS_DIR: &str = "/opt/terrarium/defaults";

/// `manifest.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// The path of each file, relative to the defaults dir, and the
    /// SHA-256 of its content (hex).
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    version: u32,
    files: BTreeMap<String, RecordEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct RecordEntry {
    /// The hash of the default that the file came from.
    default: String,
    /// The hash of a newer default that the operator did not take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    declined: Option<String>,
}

/// The SHA-256 of `data`, hex.
pub fn sha256_hex(data: &[u8]) -> String {
    crate::store::artifacts::sha256_hex(data)
}

/// A relative path with no `..`, no root, and no empty part.
fn check_rel(rel: &str) -> Result<(), String> {
    let p = Path::new(rel);
    let ok = !rel.is_empty()
        && p.components().all(|c| matches!(c, Component::Normal(_)))
        && !rel.ends_with(NEW_SUFFIX);
    if ok {
        Ok(())
    } else {
        Err(format!("`{rel}` is not a valid defaults path"))
    }
}

/// The packaged defaults: a dir and its manifest.
#[derive(Debug, Clone)]
pub struct Defaults {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// One file of the defaults.
#[derive(Debug, Clone)]
struct DefaultFile {
    rel: String,
    bytes: Vec<u8>,
    hash: String,
}

impl Defaults {
    /// Read `<dir>/manifest.json`. A file of the list that is missing, or
    /// whose hash differs from the manifest, is an error: the package is
    /// broken (`just defaults::manifest` writes the manifest again).
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join(MANIFEST);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let manifest: Manifest =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let d = Self {
            dir: dir.to_path_buf(),
            manifest,
        };
        d.files()?;
        Ok(d)
    }

    fn files(&self) -> Result<Vec<DefaultFile>, String> {
        let mut out = Vec::new();
        for (rel, want) in &self.manifest.files {
            check_rel(rel)?;
            let path = self.dir.join(rel);
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let hash = sha256_hex(&bytes);
            if &hash != want {
                return Err(format!(
                    "{}: the hash differs from {MANIFEST}; run `just defaults::manifest`",
                    path.display()
                ));
            }
            out.push(DefaultFile {
                rel: rel.clone(),
                bytes,
                hash,
            });
        }
        Ok(out)
    }

    /// The text of the default `rel`, when the manifest has it.
    pub fn text(&self, rel: &str) -> Option<String> {
        self.manifest.files.get(rel)?;
        std::fs::read_to_string(self.dir.join(rel)).ok()
    }
}

/// Make the manifest of the files in `dir` (every file, at any depth,
/// except the manifest itself and hidden files).
pub fn make_manifest(dir: &Path) -> Result<Manifest, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<(), String> {
        let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned();
                if rel == MANIFEST {
                    continue;
                }
                let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                out.insert(rel, sha256_hex(&bytes));
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(dir, dir, &mut files)?;
    Ok(Manifest { version: 1, files })
}

/// The manifest as `manifest.json` holds it.
pub fn manifest_text(m: &Manifest) -> String {
    let mut s = serde_json::to_string_pretty(m).unwrap_or_default();
    s.push('\n');
    s
}

fn read_record(target: &Path) -> Record {
    std::fs::read_to_string(target.join(RECORD))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_record(target: &Path, r: &Record) -> Result<(), String> {
    let mut r = r.clone();
    r.version = 1;
    let mut text = serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?;
    text.push('\n');
    write_atomic(&target.join(RECORD), text.as_bytes())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", ulid::Ulid::new()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn new_path(target: &Path, rel: &str) -> PathBuf {
    target.join(format!("{rel}{NEW_SUFFIX}"))
}

/// What [`seed`] did with one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedAction {
    /// The file was missing: the default is there now.
    Seeded,
    /// The file was an unedited older default: the new default replaced it.
    Upgraded,
    /// The file is edited: the new default waits in `<file>.new`.
    Pending,
    /// The file is edited, and the operator kept it against this default.
    Kept,
    /// The file is the default.
    Current,
    /// A default template file that the operator deleted: it stays
    /// deleted.
    Removed,
}

/// A default under `templates/`: an agent template (PLAN §5b). A delete
/// of one by the operator stays, where a missing prompt comes back.
fn is_template(rel: &str) -> bool {
    rel.starts_with(&format!("{}/", terrarium_core::template_file::DIR))
}

/// One row of a seed report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seeded {
    pub path: String,
    pub action: SeedAction,
}

/// Copy the defaults into `target` by the rules of the module table.
pub fn seed(defaults: &Defaults, target: &Path) -> Result<Vec<Seeded>, String> {
    std::fs::create_dir_all(target).map_err(|e| format!("{}: {e}", target.display()))?;
    let mut record = read_record(target);
    let mut out = Vec::new();
    for f in defaults.files()? {
        let path = target.join(&f.rel);
        let newp = new_path(target, &f.rel);
        let have = match std::fs::read(&path) {
            Ok(b) => Some(sha256_hex(&b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let entry = record.files.entry(f.rel.clone()).or_default();
        let action = match have {
            // A default template that the operator deleted stays deleted.
            None if is_template(&f.rel) && !entry.default.is_empty() => SeedAction::Removed,
            None => {
                write_atomic(&path, &f.bytes)?;
                remove_if_exists(&newp)?;
                *entry = RecordEntry {
                    default: f.hash.clone(),
                    declined: None,
                };
                SeedAction::Seeded
            }
            Some(h) if h == f.hash => {
                remove_if_exists(&newp)?;
                *entry = RecordEntry {
                    default: f.hash.clone(),
                    declined: None,
                };
                SeedAction::Current
            }
            Some(h) if h == entry.default => {
                write_atomic(&path, &f.bytes)?;
                remove_if_exists(&newp)?;
                *entry = RecordEntry {
                    default: f.hash.clone(),
                    declined: None,
                };
                SeedAction::Upgraded
            }
            Some(_) if entry.declined.as_deref() == Some(f.hash.as_str()) => {
                remove_if_exists(&newp)?;
                SeedAction::Kept
            }
            Some(_) => {
                let current = std::fs::read(&newp).ok().map(|b| sha256_hex(&b));
                if current.as_deref() != Some(f.hash.as_str()) {
                    write_atomic(&newp, &f.bytes)?;
                }
                SeedAction::Pending
            }
        };
        out.push(Seeded {
            path: f.rel.clone(),
            action,
        });
    }
    write_record(target, &record)?;
    Ok(out)
}

/// A new default that waits for the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The file, relative to the config repo.
    pub path: String,
    /// The unified diff from the operator's file to the new default.
    pub diff: String,
}

/// The `.new` files of the defaults in `target`, with a diff each. Only
/// paths that the record knows count, so a stray `.new` of the operator's
/// own is not shown.
pub fn pending(target: &Path) -> Vec<Pending> {
    let record = read_record(target);
    record
        .files
        .keys()
        .filter_map(|rel| {
            let new = std::fs::read_to_string(new_path(target, rel)).ok()?;
            let old = std::fs::read_to_string(target.join(rel)).unwrap_or_default();
            Some(Pending {
                path: rel.clone(),
                diff: crate::store::wiki::unified_diff(
                    &old,
                    &new,
                    rel,
                    &format!("{rel}{NEW_SUFFIX}"),
                ),
            })
        })
        .collect()
}

fn pending_bytes(target: &Path, rel: &str) -> Result<(Record, Vec<u8>), String> {
    check_rel(rel)?;
    let record = read_record(target);
    if !record.files.contains_key(rel) {
        return Err(format!("`{rel}` is not a default file"));
    }
    let bytes = std::fs::read(new_path(target, rel))
        .map_err(|_| format!("`{rel}` has no new default waiting"))?;
    Ok((record, bytes))
}

/// Take the new default of `rel`: it replaces the operator's file.
pub fn accept(target: &Path, rel: &str) -> Result<(), String> {
    let (mut record, bytes) = pending_bytes(target, rel)?;
    write_atomic(&target.join(rel), &bytes)?;
    remove_if_exists(&new_path(target, rel))?;
    record.files.insert(
        rel.to_owned(),
        RecordEntry {
            default: sha256_hex(&bytes),
            declined: None,
        },
    );
    write_record(target, &record)
}

/// Keep the operator's file of `rel` against the new default. The next
/// start does not offer this default again; a newer one comes as a new
/// `.new`.
pub fn keep(target: &Path, rel: &str) -> Result<(), String> {
    let (mut record, bytes) = pending_bytes(target, rel)?;
    remove_if_exists(&new_path(target, rel))?;
    if let Some(e) = record.files.get_mut(rel) {
        e.declined = Some(sha256_hex(&bytes));
    }
    write_record(target, &record)
}

/// Copy `src` into `dst`, at any depth, with no symlink followed.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    let rd = std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))?;
    for e in rd.filter_map(Result::ok) {
        let ty = e.file_type().map_err(|e| e.to_string())?;
        let to = dst.join(e.file_name());
        if ty.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(e.path(), &to).map_err(|err| format!("{}: {err}", to.display()))?;
        } else if ty.is_symlink()
            && let Ok(link) = std::fs::read_link(e.path())
        {
            std::os::unix::fs::symlink(link, &to)
                .map_err(|err| format!("{}: {err}", to.display()))?;
        }
    }
    Ok(())
}

/// `just defaults::apply`: copy `target` to `<backup_root>/<time>/`, then
/// write every default over it, drop the `.new` files, and record the
/// defaults as unedited. Other files (the operator's own prompts, the
/// export, `.git`) stay. The answer is the backup dir.
pub fn apply(defaults: &Defaults, target: &Path, backup_root: &Path) -> Result<PathBuf, String> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let backup = backup_root.join(format!("config-terrarium-{stamp}"));
    if target.exists() {
        copy_tree(target, &backup)?;
    } else {
        std::fs::create_dir_all(&backup).map_err(|e| format!("{}: {e}", backup.display()))?;
    }
    std::fs::create_dir_all(target).map_err(|e| format!("{}: {e}", target.display()))?;
    let mut record = Record::default();
    for f in defaults.files()? {
        write_atomic(&target.join(&f.rel), &f.bytes)?;
        remove_if_exists(&new_path(target, &f.rel))?;
        record.files.insert(
            f.rel.clone(),
            RecordEntry {
                default: f.hash.clone(),
                declined: None,
            },
        );
    }
    write_record(target, &record)?;
    Ok(backup)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn pack(dir: &Path, files: &[(&str, &str)]) -> Defaults {
        for (rel, text) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let m = make_manifest(dir).unwrap();
        std::fs::write(dir.join(MANIFEST), manifest_text(&m)).unwrap();
        Defaults::load(dir).unwrap()
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn missing_files_are_seeded_and_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let d = pack(
            &tmp.path().join("d1"),
            &[
                ("prompts/leader.md", "L1"),
                ("harness/pi/settings.json", "{}"),
            ],
        );
        let target = tmp.path().join("cfg");
        let r = seed(&d, &target).unwrap();
        assert!(r.iter().all(|s| s.action == SeedAction::Seeded), "{r:?}");
        assert_eq!(read(&target.join("prompts/leader.md")), "L1");
        assert!(target.join(RECORD).is_file());
        // A second start changes nothing.
        let r = seed(&d, &target).unwrap();
        assert!(r.iter().all(|s| s.action == SeedAction::Current), "{r:?}");
    }

    #[test]
    fn an_unedited_file_is_upgraded_and_an_edited_one_gets_a_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("cfg");
        let v1 = pack(
            &tmp.path().join("v1"),
            &[("prompts/leader.md", "L1\n"), ("prompts/worker.md", "W1\n")],
        );
        seed(&v1, &target).unwrap();
        std::fs::write(target.join("prompts/worker.md"), "W1 edited\n").unwrap();
        let v2 = pack(
            &tmp.path().join("v2"),
            &[("prompts/leader.md", "L2\n"), ("prompts/worker.md", "W2\n")],
        );
        let r = seed(&v2, &target).unwrap();
        let action = |p: &str| r.iter().find(|s| s.path == p).unwrap().action;
        assert_eq!(action("prompts/leader.md"), SeedAction::Upgraded);
        assert_eq!(action("prompts/worker.md"), SeedAction::Pending);
        assert_eq!(read(&target.join("prompts/leader.md")), "L2\n");
        assert_eq!(read(&target.join("prompts/worker.md")), "W1 edited\n");
        assert_eq!(read(&target.join("prompts/worker.md.new")), "W2\n");
        let p = pending(&target);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].path, "prompts/worker.md");
        assert!(p[0].diff.contains("-W1 edited"), "{}", p[0].diff);
        assert!(p[0].diff.contains("+W2"), "{}", p[0].diff);

        // Keep: no `.new` at the next start for this default.
        keep(&target, "prompts/worker.md").unwrap();
        assert!(!target.join("prompts/worker.md.new").exists());
        let r = seed(&v2, &target).unwrap();
        assert!(r.iter().any(|s| s.action == SeedAction::Kept));
        assert!(pending(&target).is_empty());

        // A newer default comes as a new `.new`; accept takes it.
        let v3 = pack(
            &tmp.path().join("v3"),
            &[("prompts/leader.md", "L2\n"), ("prompts/worker.md", "W3\n")],
        );
        seed(&v3, &target).unwrap();
        assert_eq!(read(&target.join("prompts/worker.md.new")), "W3\n");
        accept(&target, "prompts/worker.md").unwrap();
        assert_eq!(read(&target.join("prompts/worker.md")), "W3\n");
        assert!(pending(&target).is_empty());
        // Now unedited: a later default upgrades it.
        let v4 = pack(
            &tmp.path().join("v4"),
            &[("prompts/leader.md", "L2\n"), ("prompts/worker.md", "W4\n")],
        );
        seed(&v4, &target).unwrap();
        assert_eq!(read(&target.join("prompts/worker.md")), "W4\n");

        assert!(accept(&target, "prompts/worker.md").is_err());
        assert!(accept(&target, "../x").is_err());
    }

    /// A file that another instance changed to the new default (a sync)
    /// counts as current.
    #[test]
    fn a_file_equal_to_the_new_default_is_current() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("cfg");
        let v1 = pack(&tmp.path().join("v1"), &[("a.md", "1")]);
        seed(&v1, &target).unwrap();
        std::fs::write(target.join("a.md"), "2").unwrap();
        let v2 = pack(&tmp.path().join("v2"), &[("a.md", "2")]);
        let r = seed(&v2, &target).unwrap();
        assert_eq!(r[0].action, SeedAction::Current);
        assert!(!target.join("a.md.new").exists());
    }

    #[test]
    fn apply_backs_up_and_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("cfg");
        let d = pack(&tmp.path().join("d"), &[("prompts/leader.md", "L\n")]);
        seed(&d, &target).unwrap();
        std::fs::write(target.join("prompts/leader.md"), "mine\n").unwrap();
        std::fs::write(target.join("prompts/leader.md.new"), "L\n").unwrap();
        std::fs::write(target.join("prompts/own.md"), "own\n").unwrap();
        let backup = apply(&d, &target, &tmp.path().join("state/backup")).unwrap();
        assert!(backup.starts_with(tmp.path().join("state/backup")));
        assert_eq!(read(&backup.join("prompts/leader.md")), "mine\n");
        assert_eq!(read(&target.join("prompts/leader.md")), "L\n");
        assert!(!target.join("prompts/leader.md.new").exists());
        assert_eq!(read(&target.join("prompts/own.md")), "own\n");
        assert!(pending(&target).is_empty());
    }

    #[test]
    fn a_broken_package_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("d");
        pack(&d, &[("a.md", "1")]);
        std::fs::write(d.join("a.md"), "changed").unwrap();
        let e = Defaults::load(&d).unwrap_err();
        assert!(e.contains("just defaults::manifest"), "{e}");
        std::fs::write(d.join(MANIFEST), r#"{"version":1,"files":{"../x":"00"}}"#).unwrap();
        assert!(Defaults::load(&d).is_err());
    }

    /// The packaged defaults pass the secret scan: the first commit of a
    /// config repo is never blocked by them.
    #[test]
    fn the_packaged_defaults_pass_the_secret_scan() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../defaults");
        let d = Defaults::load(&dir).unwrap();
        for f in d.files().unwrap() {
            let found = terrarium_core::secret_scan::scan(&f.rel, &f.bytes);
            assert!(found.is_empty(), "{found:?}");
        }
    }

    /// The repo's `defaults/manifest.json` matches the files.
    #[test]
    fn the_packaged_manifest_is_current() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../defaults");
        let have: Manifest =
            serde_json::from_str(&std::fs::read_to_string(dir.join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(
            have,
            make_manifest(&dir).unwrap(),
            "run `just defaults::manifest`"
        );
        let d = Defaults::load(&dir).unwrap();
        for rel in [
            "README.md",
            "prompts/leader.md",
            "prompts/worker.md",
            "prompts/default.md",
            "prompts/project-lead.md",
            "prompts/system-lead.md",
            "prompts/harness/claude.md",
            "prompts/harness/codex.md",
            "prompts/harness/copilot.md",
            "prompts/harness/pi.md",
            "harness/claude/settings.json",
            "harness/codex/config.toml",
            "harness/copilot/settings.json",
            "harness/pi/settings.json",
        ] {
            assert!(d.manifest.files.contains_key(rel), "{rel}");
        }
    }
}
