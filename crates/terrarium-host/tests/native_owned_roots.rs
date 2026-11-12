//! Privileged filesystem boundary, with two real pool UIDs and no GPU.
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use terrarium_host::native::{OwnedRoot, provision_root, remove_empty_root};

fn request(base: &Path, name: &str) -> OwnedRoot {
    OwnedRoot {
        cleanup: false,
        base: base.to_str().unwrap().into(),
        project: "project".into(),
        name: name.into(),
        agent: format!("id-{name}"),
    }
}

fn workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::os::unix::fs::chown(tmp.path(), Some(0), Some(23000)).unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    tmp
}

fn pool_session(base: &Path, name: &str, uid: u32) {
    let out = Command::new("python3")
        .args(["-c", r#"
import os, sys
base, name, sibling = sys.argv[1:]
os.chdir(base + '/project/' + name)
with open('work-' + str(os.getuid()), 'w') as out: out.write('work')
for op in [lambda: os.chmod(base + '/project', 0o777),
           lambda: os.rename(base + '/project/' + sibling, base + '/project/stolen'),
           lambda: os.rename(base + '/project', base + '/stolen')]:
    try: op()
    except PermissionError: pass
    else: raise AssertionError('pool user modified protected parent')
assert os.path.isfile(base + '/project/' + sibling + '/.terrarium-owner')
"#])
        .arg(base)
        .arg(name)
        .arg(if name == "a" { "b" } else { "a" })
        .uid(uid)
        .gid(23000)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn two_pool_users_cannot_change_project_or_rename_sibling_roots() {
    if !nix::unistd::geteuid().is_root() {
        eprintln!("requires root to exercise two real pool UIDs");
        return;
    }
    let tmp = workspace();
    for name in ["a", "b"] {
        provision_root(tmp.path(), &request(tmp.path(), name)).unwrap();
    }
    let project = tmp.path().join("project");
    assert_eq!(project.metadata().unwrap().uid(), 0);
    assert_eq!(project.metadata().unwrap().mode() & 0o022, 0);
    for (name, uid) in [("a", 23001), ("b", 23002), ("a", 23002), ("b", 23001)] {
        pool_session(tmp.path(), name, uid);
        provision_root(tmp.path(), &request(tmp.path(), name)).unwrap();
    }
    assert!(project.join("a/work-23001").exists());
    assert!(project.join("b/work-23002").exists());
}

#[test]
fn unsafe_existing_parents_are_refused_without_repair() {
    if !nix::unistd::geteuid().is_root() { return; }
    let tmp = workspace();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    for (path, uid, mode) in [
        (project.as_path(), 23001, 0o755),
        (project.as_path(), 0, 0o775),
        (tmp.path(), 23001, 0o755),
        (tmp.path(), 0, 0o777),
    ] {
        std::os::unix::fs::chown(path, Some(uid), None).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        let err = provision_root(tmp.path(), &request(tmp.path(), "a")).unwrap_err();
        assert!(err.message.contains("administrator migration required"), "{err:?}");
        assert_eq!(path.metadata().unwrap().uid(), uid);
        assert_eq!(path.metadata().unwrap().mode() & 0o777, mode);
        assert!(!project.join("a").exists());
        std::os::unix::fs::chown(path, Some(0), None).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn cleanup_removes_only_empty_matching_root_and_preserves_sibling() {
    if !nix::unistd::geteuid().is_root() { return; }
    let tmp = workspace();
    let a = request(tmp.path(), "a");
    let b = request(tmp.path(), "b");
    provision_root(tmp.path(), &a).unwrap();
    provision_root(tmp.path(), &b).unwrap();
    let mut wrong = a.clone();
    wrong.agent = b.agent.clone();
    assert!(remove_empty_root(tmp.path(), &wrong).is_err());
    std::fs::write(tmp.path().join("project/a/keep"), "data").unwrap();
    assert!(remove_empty_root(tmp.path(), &a).is_err());
    assert!(tmp.path().join("project/a/.terrarium-owner").exists());
    std::fs::remove_file(tmp.path().join("project/a/keep")).unwrap();
    remove_empty_root(tmp.path(), &a).unwrap();
    assert!(!tmp.path().join("project/a").exists());
    assert!(tmp.path().join("project/b/.terrarium-owner").exists());
}

#[test]
fn symlinks_and_unmarked_existing_roots_are_refused() {
    if !nix::unistd::geteuid().is_root() { return; }
    let tmp = workspace();
    let other = workspace();
    std::os::unix::fs::symlink(other.path(), tmp.path().join("project")).unwrap();
    assert!(provision_root(tmp.path(), &request(tmp.path(), "a")).is_err());
    assert!(std::fs::read_dir(other.path()).unwrap().next().is_none());
    std::fs::remove_file(tmp.path().join("project")).unwrap();
    std::fs::create_dir_all(tmp.path().join("project/a")).unwrap();
    assert!(provision_root(tmp.path(), &request(tmp.path(), "a")).is_err());
    let mut bad = request(tmp.path(), "a");
    bad.project = "..".into();
    assert!(provision_root(tmp.path(), &bad).is_err());
}
