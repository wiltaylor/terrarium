//! Explicit-only synthetic kernel proof. Never run on a shared build host.
use super::*;
use super::mounts::{Attributes, LinuxAttributes};
use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use terrarium_core::live_workspace::{NamespaceIdentity, SourceIdentity, RuntimeIdentity};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}
struct Cleanup(Vec<File>);
impl Drop for Cleanup {
    fn drop(&mut self) { for mount in self.0.iter().rev() { let _ = mounts::lazy_detach(mount); } }
}

fn ns(path: &str) -> NamespaceIdentity {
    let m = std::fs::metadata(path).unwrap();
    NamespaceIdentity { device: m.dev(), inode: m.ino() }
}
fn host_view() -> Vec<terrarium_core::live_workspace::MountEvidence> {
    mountinfo::parse(&std::fs::read("/proc/self/mountinfo").unwrap(), ns("/proc/self/ns/mnt")).unwrap()
}
fn entry(path: &Path) -> terrarium_core::live_workspace::MountEvidence {
    host_view().into_iter().find(|m| Path::new(&m.mountpoint) == path).unwrap()
}
fn isolate(path: &Path, cleanup: &mut Cleanup, shared: bool) -> File {
    let underlying = File::open(path).unwrap();
    let tree = mounts::clone_nonrecursive(&underlying).unwrap();
    mounts::attach(&tree, &underlying).unwrap();
    let mounted = File::open(path).unwrap();
    cleanup.0.push(mounted.try_clone().unwrap());
    mounts::make_private(&mounted).unwrap();
    assert!(entry(path).optional_fields.is_empty());
    if shared { mounts::make_shared(&mounted).unwrap(); }
    mounted
}

struct Receiver { snapshot: runtime::Snapshot }
impl runtime::Inspector for Receiver {
    fn inspect(&mut self, _: &str) -> Result<runtime::Snapshot, Reply> { Ok(self.snapshot.clone()) }
}

#[test]
#[ignore = "requires explicit disposable VM and a private outer mount namespace"]
fn kernel_publish_revoke_and_refusals() {
    assert_eq!(std::env::var("TERRARIUM_B2_DISPOSABLE").as_deref(), Ok("1"));
    assert!(nix::unistd::geteuid().is_root());
    assert_ne!(ns("/proc/self/ns/mnt"), ns("/proc/1/ns/mnt"), "launch this fixture with unshare --mount --propagation private");
    let fixture = tempfile::Builder::new().prefix("ter925-fixture-").tempdir_in("/root").unwrap();
    let mut cleanup = Cleanup(vec![]);
    for name in ["source", "staging", "publisher", "receiver", "other"] { std::fs::create_dir(fixture.path().join(name)).unwrap(); }
    let source_path = fixture.path().join("source");
    let staging_path = fixture.path().join("staging");
    let publisher_path = fixture.path().join("publisher");
    let receiver_path = fixture.path().join("receiver");
    let source = isolate(&source_path, &mut cleanup, true);
    let _staging = isolate(&staging_path, &mut cleanup, false);
    let publisher = isolate(&publisher_path, &mut cleanup, true);
    LinuxAttributes.scaffold(&publisher).unwrap();
    std::fs::create_dir(publisher_path.join("reverse")).unwrap();
    std::fs::write(fixture.path().join("other/reverse-canary"), b"receiver-only").unwrap();

    let ready = fixture.path().join("receiver-ready");
    // Test harness only. Fixed absolute binaries and positional synthetic paths.
    // Production helper code never shells out to mount or accepts shell text.
    let mut child = ChildGuard(Command::new("/usr/bin/unshare")
        .args(["--mount", "--propagation", "unchanged", "/bin/sh", "-c",
            "/usr/bin/mount --bind \"$1\" \"$2\" && /usr/bin/mount --make-rslave \"$2\" && /usr/bin/mount -o remount,bind,ro \"$2\" && /usr/bin/touch \"$3\" && exec /usr/bin/sleep 600", "fixture"])
        .arg(&publisher_path).arg(&receiver_path).arg(&ready).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(child.0.try_wait().unwrap().is_none(), "receiver setup failed");
        assert!(Instant::now() < deadline, "receiver setup deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = child.0.id();
    let mut identity: RuntimeIdentity = super::tests::envelope(1, Kind::Grant).original.runtime;
    identity.namespace = ns(&format!("/proc/{pid}/ns/mnt"));
    let mut inspector = Receiver { snapshot: runtime::Snapshot { identity: identity.clone(), pid, started_at: "synthetic-runtime".into(), running: true } };
    let pin = runtime::PinnedRuntime::acquire(&mut inspector, &identity).unwrap();
    let receiver_entry = |inspector: &mut Receiver| pin.mountinfo(inspector).unwrap().into_iter().find(|m| Path::new(&m.mountpoint) == receiver_path).unwrap();
    let host_publisher = entry(&publisher_path);
    mountinfo::publisher_fixture(&host_publisher, &receiver_entry(&mut inspector), &host_view(), receiver_path.to_str().unwrap()).unwrap();
    let receiver_root = std::path::PathBuf::from(format!("/proc/{pid}/root"));
    let receiver_lookup = receiver_root.join(receiver_path.strip_prefix("/").unwrap());
    assert!(File::create(receiver_lookup.join("scaffold-write")).is_err());

    let source_root = ProtectedRoot::open(&source_path).unwrap();
    let staging_root = ProtectedRoot::open(&staging_path).unwrap();
    let publisher_root = ProtectedRoot::open(&publisher_path).unwrap();
    std::fs::create_dir(source_path.join("project")).unwrap();
    let mut destinations = Vec::new();
    for name in ["agent", "agent2"] {
        let workspace = source_path.join("project").join(name);
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("canary"), name.as_bytes()).unwrap();
        std::fs::write(workspace.join("run"), b"#!/bin/sh\nexit 17\n").unwrap();
        std::fs::set_permissions(workspace.join("run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut enrollment = super::tests::envelope(1, Kind::Grant).original.holder.unwrap().enrollment;
        enrollment.name = name.into();
        let m = std::fs::metadata(&workspace).unwrap();
        enrollment.source = SourceIdentity { device: m.dev(), inode: m.ino() };
        let opened = source_root.enrolled(&enrollment).unwrap();
        scan_source(&opened).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(workspace.join("socket")).unwrap();
        assert_eq!(scan_source(&opened), Err(Reply::Unauthorized));
        drop(socket); std::fs::remove_file(workspace.join("socket")).unwrap();
        std::fs::create_dir(workspace.join("nested")).unwrap();
        let nested_underlying = File::open(workspace.join("nested")).unwrap();
        let foreign = mounts::clone_nonrecursive(&File::open(fixture.path().join("other")).unwrap()).unwrap();
        mounts::attach(&foreign, &nested_underlying).unwrap();
        let nested = File::open(workspace.join("nested")).unwrap();
        assert_eq!(scan_source(&opened), Err(Reply::Unauthorized));
        mounts::lazy_detach(&nested).unwrap();
        scan_source(&opened).unwrap();

        let stage = staging_root.destination(&enrollment).unwrap();
        let destination = publisher_root.destination(&enrollment).unwrap();
        let tree = mounts::clone_nonrecursive(&opened).unwrap();
        mounts::attach(&tree, stage.placeholder().unwrap()).unwrap();
        let staged = stage.mounted().unwrap();
        cleanup.0.push(staged.try_clone().unwrap());
        mounts::make_shared(&staged).unwrap();
        LinuxAttributes.workspace(&staged).unwrap();
        let stage_path = staging_path.join("project").join(name);
        let expected_root = format!("{}/project/{name}", entry(&source_path).root);
        assert!(mountinfo::private_stage(&entry(&stage_path), &expected_root, &entry(&source_path).major_minor).is_err());
        mounts::make_private(&staged).unwrap();
        mountinfo::private_stage(&entry(&stage_path), &expected_root, &entry(&source_path).major_minor).unwrap();
        mounts::attach(&staged, destination.placeholder().unwrap()).unwrap();
        stage.remove().unwrap();
        let host_mount = entry(&publisher_path.join("project").join(name));
        let receiver_mount = pin.mountinfo(&mut inspector).unwrap().into_iter().find(|m| Path::new(&m.mountpoint) == receiver_path.join("project").join(name)).unwrap();
        mountinfo::workspace(&host_mount, &receiver_mount, &host_publisher, &receiver_entry(&mut inspector), &expected_root, &entry(&source_path).major_minor).unwrap();
        assert_eq!(std::fs::read(receiver_lookup.join("project").join(name).join("canary")).unwrap(), name.as_bytes());
        std::fs::write(receiver_lookup.join("project").join(name).join("writable"), b"yes").unwrap();
        assert_eq!(Command::new(receiver_lookup.join("project").join(name).join("run")).status().unwrap().code(), Some(17));
        destinations.push(destination);
    }
    let revoked = destinations.remove(0);
    let mount = revoked.mounted().unwrap();
    if mounts::ordinary_unmount(&mount).unwrap() == mounts::Unmount::Busy { mounts::lazy_detach(&mount).unwrap(); }
    revoked.remove().unwrap();
    assert_eq!(std::fs::metadata(receiver_lookup.join("project/agent")).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(std::fs::read(receiver_lookup.join("project/agent2/canary")).unwrap(), b"agent2");
    assert!(child.0.try_wait().unwrap().is_none());
    // Reverse propagation: a receiver-local mount must not appear on publisher.
    let status = Command::new("/usr/bin/nsenter").arg(format!("--mount=/proc/{pid}/ns/mnt"))
        .args(["/usr/bin/mount", "--bind"]).arg(fixture.path().join("other")).arg(receiver_path.join("reverse")).status().unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(receiver_lookup.join("reverse/reverse-canary")).unwrap(), b"receiver-only");
    assert!(!publisher_path.join("reverse/reverse-canary").exists());
    assert!(!host_view().iter().any(|m| Path::new(&m.mountpoint) == publisher_path.join("reverse")));
    assert_eq!(mount_id(&source).unwrap(), mount_id(&source_root.fd).unwrap());
    drop(child); // stop only the exact child started by this fixture
    drop(cleanup); // detach only fixture-owned pinned mounts before tmp cleanup
}
