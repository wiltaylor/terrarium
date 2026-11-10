//! Round trips: full, diff, sealed, and the checks of a restore.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::fs;

struct Fixture {
    _tmp: tempfile::TempDir,
    state: PathBuf,
    config: PathBuf,
    work: PathBuf,
    dir: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let config = tmp.path().join("config");
    let work = tmp.path().join("work");
    let dir = state.join("backups");
    fs::create_dir_all(state.join("agents/A1/private")).unwrap();
    fs::set_permissions(
        state.join("agents/A1/private"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(state.join("agents/A1/private/events.jsonl"), "{}\n").unwrap();
    // A card file blob (docs/api.md §8.4): the state root has the store.
    fs::create_dir_all(state.join("card-files")).unwrap();
    fs::write(state.join("card-files").join("a".repeat(64)), "# brief").unwrap();
    fs::write(state.join("secrets.key"), [7u8; 32]).unwrap();
    fs::set_permissions(state.join("secrets.key"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir_all(state.join("credentials/accounts/claude/a")).unwrap();
    fs::write(
        state.join("credentials/accounts/claude/a/.credentials.json"),
        "tok",
    )
    .unwrap();
    fs::create_dir_all(state.join("agents/A1/harness/cred")).unwrap();
    std::os::unix::fs::symlink(
        "../../../../credentials/accounts/claude/a",
        state.join("agents/A1/harness/cred/account"),
    )
    .unwrap();
    // The database, in WAL mode, as the daemon has it.
    let c = rusqlite::Connection::open(state.join(DB_FILE)).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.execute_batch(
        "CREATE TABLE t(v TEXT); INSERT INTO t VALUES ('one'); PRAGMA user_version = 57;",
    )
    .unwrap();
    drop(c);
    fs::create_dir_all(config.join("prompts")).unwrap();
    fs::write(config.join("prompts/lead.md"), "lead").unwrap();
    fs::create_dir_all(work.join("p/a/target/debug")).unwrap();
    fs::write(work.join("p/a/main.rs"), "fn main() {}").unwrap();
    fs::write(work.join("p/a/target/debug/big"), vec![0u8; 1000]).unwrap();
    fs::create_dir_all(work.join("p/a/node_modules/x")).unwrap();
    fs::write(work.join("p/a/node_modules/x/i.js"), "x").unwrap();
    Fixture {
        _tmp: tmp,
        state,
        config,
        work,
        dir,
    }
}

impl Fixture {
    fn roots(&self, work: bool) -> Roots {
        Roots {
            state: self.state.clone(),
            config: Some(self.config.clone()),
            work: work.then(|| self.work.clone()),
        }
    }

    fn make(&self, kind: Kind, work: bool, pass: Option<&str>) -> Created {
        create(
            &self.roots(work),
            &CreateOptions {
                kind,
                dir: self.dir.clone(),
                base: None,
                passphrase: pass.map(str::to_owned),
                db: DbSource::File(self.state.join(DB_FILE)),
            },
        )
        .unwrap()
    }

    fn db_value(&self) -> Vec<String> {
        let c = rusqlite::Connection::open(self.state.join(DB_FILE)).unwrap();
        let mut s = c.prepare("SELECT v FROM t ORDER BY v").unwrap();
        s.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn set_db(&self, sql: &str) {
        let c = rusqlite::Connection::open(self.state.join(DB_FILE)).unwrap();
        c.execute_batch(sql).unwrap();
    }

    fn restore(&self, c: &Created, pass: Option<&str>) -> Restored {
        restore(
            &c.path,
            &self.roots(true),
            &RestoreOptions {
                passphrase: pass.map(str::to_owned),
                keep: vec![self.dir.clone()],
                ..RestoreOptions::default()
            },
        )
        .unwrap()
    }
}

fn names(c: &Created) -> Vec<String> {
    let f = File::open(&c.path).unwrap();
    let z = ZipArchive::new(f).unwrap();
    z.file_names().map(|n| n.unwrap().into_owned()).collect()
}

#[test]
fn a_full_backup_round_trips() {
    let fx = fixture();
    let full = fx.make(Kind::Full, true, None);
    assert_eq!(full.header.kind, Kind::Full);
    assert!(full.header.contains_secrets);
    assert_eq!(full.header.db_user_version, 57);
    assert_eq!(full.header.roots, ["state", "config", "work"]);
    let n = names(&full);
    assert!(n.contains(&"README.txt".to_owned()));
    assert!(n.contains(&"files/state/terrarium.db".to_owned()));
    assert!(n.contains(&"files/state/secrets.key".to_owned()));
    assert!(n.contains(&format!("files/state/card-files/{}", "a".repeat(64))));
    assert!(n.contains(&"files/work/p/a/main.rs".to_owned()));
    // No live DB files, no caches, no build output, no backups in it.
    assert!(!n.iter().any(|x| x.contains("-wal") || x.contains("-shm")));
    assert!(
        !n.iter()
            .any(|x| x.contains("target/") || x.contains("node_modules"))
    );
    assert!(!n.iter().any(|x| x.contains("backups/")));
    assert_eq!(
        fs::metadata(&full.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let v = verify(&full.path, None, None).unwrap();
    assert_eq!(v.checked_files, full.header.stored_files);

    // Change everything, then restore.
    fx.set_db("INSERT INTO t VALUES ('two');");
    fs::write(fx.state.join("secrets.key"), [9u8; 32]).unwrap();
    fs::write(fx.config.join("prompts/new.md"), "new").unwrap();
    fs::remove_file(fx.work.join("p/a/main.rs")).unwrap();
    let r = fx.restore(&full, None);
    assert_eq!(r.roots.len(), 3);
    assert_eq!(fx.db_value(), ["one"]);
    assert_eq!(fs::read(fx.state.join("secrets.key")).unwrap(), [7u8; 32]);
    assert!(!fx.config.join("prompts/new.md").exists());
    assert_eq!(
        fs::read_to_string(fx.work.join("p/a/main.rs")).unwrap(),
        "fn main() {}"
    );
    assert_eq!(
        fs::read_link(fx.state.join("agents/A1/harness/cred/account")).unwrap(),
        Path::new("../../../../credentials/accounts/claude/a")
    );
    assert_eq!(
        fs::metadata(fx.state.join("agents/A1/private"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    // The old content is aside, and the backups stay in place.
    let prev = r.roots[0].previous.clone().unwrap();
    assert_eq!(fs::read(prev.join("secrets.key")).unwrap(), [9u8; 32]);
    assert!(full.path.exists());
    assert!(
        !fx.state
            .join(format!("{STAGING_PREFIX}{}", full.header.id))
            .exists()
    );
}

#[test]
fn a_diff_stores_the_changes_and_restores_the_exact_tree() {
    let fx = fixture();
    let full = fx.make(Kind::Full, false, None);
    // A change, a new file, and a deleted file.
    fs::write(fx.config.join("prompts/lead.md"), "lead v2").unwrap();
    fs::write(fx.state.join("agents/A1/private/new.txt"), "n").unwrap();
    fs::remove_file(
        fx.state
            .join("credentials/accounts/claude/a/.credentials.json"),
    )
    .unwrap();
    fx.set_db("INSERT INTO t VALUES ('two');");
    let diff = fx.make(Kind::Diff, false, None);
    assert_eq!(diff.header.kind, Kind::Diff);
    assert_eq!(diff.header.base.as_ref().unwrap().id, full.header.id);
    let n = names(&diff);
    assert!(n.contains(&"files/config/prompts/lead.md".to_owned()));
    assert!(n.contains(&"files/state/agents/A1/private/new.txt".to_owned()));
    assert!(n.contains(&"files/state/terrarium.db".to_owned()));
    // An unchanged file stays in the base only.
    assert!(!n.contains(&"files/state/secrets.key".to_owned()));
    assert!(diff.header.stored_files < full.header.stored_files);
    verify(&diff.path, None, None).unwrap();

    // Change again, then restore the diff (with its base).
    fs::write(fx.config.join("prompts/lead.md"), "lead v3").unwrap();
    fx.set_db("INSERT INTO t VALUES ('three');");
    let r = fx.restore(&diff, None);
    assert_eq!(r.base.as_deref(), Some(full.header.id.as_str()));
    assert_eq!(fx.db_value(), ["one", "two"]);
    assert_eq!(
        fs::read_to_string(fx.config.join("prompts/lead.md")).unwrap(),
        "lead v2"
    );
    assert_eq!(fs::read(fx.state.join("secrets.key")).unwrap(), [7u8; 32]);
    assert!(fx.state.join("agents/A1/private/new.txt").exists());
    assert!(
        !fx.state
            .join("credentials/accounts/claude/a/.credentials.json")
            .exists()
    );
    // A work root that the backup does not have stays as it is.
    assert!(fx.work.join("p/a/main.rs").exists());
}

#[test]
fn a_diff_with_no_base_is_an_error() {
    let fx = fixture();
    let err = create(
        &fx.roots(false),
        &CreateOptions {
            kind: Kind::Diff,
            dir: fx.dir.clone(),
            base: None,
            passphrase: None,
            db: DbSource::File(fx.state.join(DB_FILE)),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("no full backup"), "{err}");
}

#[test]
fn a_sealed_backup_needs_the_passphrase() {
    let fx = fixture();
    fs::write(fx.config.join("prompts/lead.md"), "PLAIN-TEXT-MARKER-1234").unwrap();
    let full = fx.make(Kind::Full, false, Some("correct horse"));
    let diff = fx.make(Kind::Diff, false, Some("correct horse"));
    assert!(full.header.encryption.is_some());
    // The content does not read without the key: no plain text in the zip
    // (the entry names are visible, as docs/api.md §32.2 says).
    let raw = fs::read(&full.path).unwrap();
    assert!(!raw.windows(22).any(|w| w == b"PLAIN-TEXT-MARKER-1234"));
    assert!(!raw.windows(32).any(|w| w == [7u8; 32]));
    let e = Archive::open(&full.path, None).unwrap_err();
    assert!(e.downcast_ref::<NeedsPassphrase>().is_some());
    assert!(Archive::open(&full.path, Some("wrong")).is_err());
    verify(&diff.path, None, Some("correct horse")).unwrap();
    fs::write(fx.state.join("secrets.key"), [1u8; 32]).unwrap();
    fx.restore(&diff, Some("correct horse"));
    assert_eq!(fs::read(fx.state.join("secrets.key")).unwrap(), [7u8; 32]);
}

#[test]
fn a_bad_file_stops_the_restore_and_changes_nothing() {
    let fx = fixture();
    let full = fx.make(Kind::Full, false, None);
    // Copy the zip with one entry changed (the same name and size).
    let bad = fx.dir.join("bad.zip");
    {
        let mut src = ZipArchive::new(File::open(&full.path).unwrap()).unwrap();
        let mut out = ZipWriter::new(File::create(&bad).unwrap());
        for i in 0..src.len() {
            let mut f = src.by_index(i).unwrap();
            let name = f.name().unwrap().into_owned();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            if name == "files/config/prompts/lead.md" {
                data = b"evil".to_vec();
            }
            out.start_file(name, SimpleFileOptions::default()).unwrap();
            out.write_all(&data).unwrap();
        }
        out.finish().unwrap();
    }
    fs::write(fx.config.join("prompts/lead.md"), "live").unwrap();
    assert!(verify(&bad, None, None).is_err());
    let err = restore(&bad, &fx.roots(false), &RestoreOptions::default()).unwrap_err();
    assert!(format!("{err:#}").contains("does not match"), "{err:#}");
    assert_eq!(
        fs::read_to_string(fx.config.join("prompts/lead.md")).unwrap(),
        "live"
    );
    assert!(
        !fx.config
            .join(format!("{STAGING_PREFIX}{}", full.header.id))
            .exists()
    );
}

#[test]
fn a_manifest_path_out_of_the_root_is_refused() {
    let m = Manifest {
        id: "X".into(),
        entries: vec![Entry {
            path: "state/../../etc/passwd".into(),
            kind: EntryKind::File,
            mode: 0o600,
            size: 0,
            mtime_ns: 0,
            sha256: None,
            link: None,
            stored_in: None,
        }],
    };
    assert!(check_paths(&m).is_err());
}

fn entry(path: &str, kind: EntryKind, link: Option<&str>) -> Entry {
    Entry {
        path: path.into(),
        kind,
        mode: 0o600,
        size: 0,
        mtime_ns: 0,
        sha256: None,
        link: link.map(str::to_owned),
        stored_in: None,
    }
}

#[test]
fn a_manifest_path_below_a_symlink_entry_is_refused() {
    // The link comes after the file in the manifest: the order does not
    // matter.
    for entries in [
        vec![
            entry("state/a", EntryKind::Symlink, Some("/etc")),
            entry("state/a/passwd", EntryKind::File, None),
        ],
        vec![
            entry("state/a/b/c", EntryKind::File, None),
            entry("state/a", EntryKind::Symlink, Some("/etc")),
        ],
        vec![
            entry("work/p", EntryKind::Symlink, Some("../../x")),
            entry("work/p/d", EntryKind::Dir, None),
        ],
    ] {
        let m = Manifest {
            id: "X".into(),
            entries,
        };
        let err = check_paths(&m).unwrap_err();
        assert!(format!("{err:#}").contains("below the symlink"), "{err:#}");
    }
    // Plain form only: `a/./x` or `a//x` would hide the link prefix.
    for path in ["state/a/./x", "state/a//x", "state/a/x/"] {
        let m = Manifest {
            id: "X".into(),
            entries: vec![
                entry("state/a", EntryKind::Symlink, Some("/etc")),
                entry(path, EntryKind::File, None),
            ],
        };
        assert!(check_paths(&m).is_err(), "{path}");
    }
    // The same path twice, of any kinds: a dir entry on a symlink entry
    // would chmod the link target.
    for kinds in [
        [EntryKind::Symlink, EntryKind::Dir],
        [EntryKind::Dir, EntryKind::Symlink],
        [EntryKind::File, EntryKind::File],
        [EntryKind::Dir, EntryKind::Dir],
    ] {
        let m = Manifest {
            id: "X".into(),
            entries: kinds
                .iter()
                .map(|k| entry("state/a", *k, Some("/root/.ssh")))
                .collect(),
        };
        let err = check_paths(&m).unwrap_err();
        assert!(format!("{err:#}").contains("twice"), "{kinds:?}: {err:#}");
    }
    // A symlink next to a file of the same prefix is fine.
    let m = Manifest {
        id: "X".into(),
        entries: vec![
            entry("state/a", EntryKind::Symlink, Some("b")),
            entry("state/ab/x", EntryKind::File, None),
            entry("state/b/x", EntryKind::File, None),
        ],
    };
    check_paths(&m).unwrap();
}

#[test]
fn a_restore_with_a_dir_entry_on_a_symlink_entry_changes_nothing() {
    let fx = fixture();
    let full = fx.make(Kind::Full, false, None);
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    // A dir outside every root, for a link with an absolute target.
    let outside = fx._tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
    let outside_link = outside.to_str().unwrap().to_owned();
    // (the symlink path, its new target when the backup does not have it,
    // the dir that the dir entry would change)
    let cases = [
        (
            "state/agents/A1/harness/cred/account",
            None,
            fx.state.join("credentials/accounts/claude/a"),
        ),
        (
            "state/agents/A1/escape",
            Some(outside_link),
            outside.clone(),
        ),
    ];
    for (n, (link, new_target, target)) in cases.into_iter().enumerate() {
        let before = mode(&target);
        // Copy the zip with a second manifest entry on the symlink: a dir of
        // mode 0777.
        let bad = fx.dir.join(format!("dup{n}.zip"));
        {
            let mut src = ZipArchive::new(File::open(&full.path).unwrap()).unwrap();
            let mut out = ZipWriter::new(File::create(&bad).unwrap());
            for i in 0..src.len() {
                let mut f = src.by_index(i).unwrap();
                let name = f.name().unwrap().into_owned();
                let mut data = Vec::new();
                f.read_to_end(&mut data).unwrap();
                if name == MANIFEST {
                    let mut m: Manifest = serde_json::from_slice(&data).unwrap();
                    match &new_target {
                        None => assert!(
                            m.entries
                                .iter()
                                .any(|e| e.path == link && e.kind == EntryKind::Symlink)
                        ),
                        Some(t) => {
                            assert!(
                                Path::new(t).is_absolute() && !Path::new(t).starts_with(&fx.state)
                            );
                            m.entries
                                .push(entry(link, EntryKind::Symlink, Some(t.as_str())));
                        }
                    }
                    let mut dir = entry(link, EntryKind::Dir, None);
                    dir.mode = 0o777;
                    m.entries.push(dir);
                    data = serde_json::to_vec(&m).unwrap();
                }
                out.start_file(name, SimpleFileOptions::default()).unwrap();
                out.write_all(&data).unwrap();
            }
            out.finish().unwrap();
        }
        let err = restore(&bad, &fx.roots(false), &RestoreOptions::default()).unwrap_err();
        assert!(format!("{err:#}").contains("twice"), "{link}: {err:#}");
        assert_eq!(mode(&target), before, "{link}");
        assert!(
            !fx.state
                .join(format!("{STAGING_PREFIX}{}", full.header.id))
                .exists()
        );
    }
}

#[test]
fn a_backup_skips_the_secret_run_dirs() {
    let fx = fixture();
    let run = crate::secrets::RunDirs::base(&fx.state).join("01RUN");
    fs::create_dir_all(&run).unwrap();
    fs::write(run.join("askpass"), "echo tok").unwrap();
    // A `.run` dir elsewhere is data.
    fs::create_dir_all(fx.work.join("p/a/.run")).unwrap();
    fs::write(fx.work.join("p/a/.run/pid"), "1").unwrap();
    let full = fx.make(Kind::Full, true, None);
    let n = names(&full);
    assert!(!n.iter().any(|x| x.contains("credentials/.run")), "{n:?}");
    assert!(n.contains(&"files/work/p/a/.run/pid".to_owned()));
    assert!(n.contains(&"files/state/credentials/accounts/claude/a/.credentials.json".to_owned()));
}

#[test]
fn list_and_prune_keep_the_newest_fulls_and_their_diffs() {
    let fx = fixture();
    let f1 = fx.make(Kind::Full, false, None);
    let d1 = fx.make(Kind::Diff, false, None);
    let f2 = fx.make(Kind::Full, false, None);
    let d2 = fx.make(Kind::Diff, false, None);
    assert_eq!(d2.header.base.as_ref().unwrap().id, f2.header.id);
    let l = list(&fx.dir).unwrap();
    assert_eq!(l.len(), 4);
    // Nothing goes before the newest full backup is verified.
    assert!(prune(&fx.dir, 1, &f1.header.id).is_err());
    assert_eq!(list(&fx.dir).unwrap().len(), 4);
    let removed = prune(&fx.dir, 1, &f2.header.id).unwrap();
    assert_eq!(removed.len(), 2);
    assert!(removed.contains(&f1.name) && removed.contains(&d1.name));
    assert!(f2.path.exists() && d2.path.exists());
    assert!(is_backup_name(&f2.name));
    assert!(!is_backup_name("../x.zip") && !is_backup_name(".x.zip.partial"));
}

#[test]
fn the_drill_restores_a_sealed_diff_into_a_scratch_dir_and_checks_it() {
    let fx = fixture();
    fx.make(Kind::Full, true, Some("pw"));
    fs::write(fx.config.join("prompts/lead.md"), "lead v2").unwrap();
    let d = fx.make(Kind::Diff, true, Some("pw"));
    let scratch = fx.dir.join(format!("{DRILL_PREFIX}t"));
    let r = drill(&d.path, None, Some("pw"), &scratch).unwrap();
    assert_eq!(r.id, d.header.id);
    assert_eq!(r.kind, Kind::Diff);
    assert_eq!(r.integrity, "ok");
    assert_eq!(r.db_user_version, 57);
    assert!(r.secrets_key);
    assert!(r.files > 0);
    assert_eq!(
        fs::read_to_string(scratch.join("config/prompts/lead.md")).unwrap(),
        "lead v2"
    );
    let c = rusqlite::Connection::open(scratch.join("state").join(DB_FILE)).unwrap();
    let v: String = c.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(v, "one");
    // The live data did not change.
    assert_eq!(
        fs::read_to_string(fx.config.join("prompts/lead.md")).unwrap(),
        "lead v2"
    );
    assert!(fs::read_dir(&fx.state).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(PRE_RESTORE_PREFIX)
    }));
    // A dir with content is refused; a wrong passphrase fails.
    assert!(drill(&d.path, None, Some("pw"), &scratch).is_err());
    let other = fx.dir.join(format!("{DRILL_PREFIX}u"));
    assert!(drill(&d.path, None, Some("bad"), &other).is_err());
    // A drill dir is not a backup in the list.
    assert!(
        list(&fx.dir)
            .unwrap()
            .iter()
            .all(|l| !l.name.starts_with('.'))
    );
}

#[test]
fn the_drill_refuses_a_dir_it_cannot_read() {
    let tmp = tempfile::tempdir().unwrap();
    // Absent and empty dirs pass.
    check_drill_dir(&tmp.path().join("none")).unwrap();
    let empty = tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    check_drill_dir(&empty).unwrap();
    // A file is not a drill dir.
    let file = tmp.path().join("file");
    fs::write(&file, "mine").unwrap();
    assert!(check_drill_dir(&file).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), "mine");
    // A dir that cannot be read may hold files: refused, not deleted.
    let locked = tmp.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("mine"), "x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    // Root reads it anyway; then the dir is not empty, also refused.
    let e = check_drill_dir(&locked).unwrap_err().to_string();
    if fs::read_dir(&locked).is_err() {
        assert!(e.contains("cannot read"), "{e}");
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(locked.join("mine").exists());
}
