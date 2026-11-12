//! Explicit-only synthetic kernel proof. Never run on a shared build host.
use super::mounts::{Attributes, LinuxAttributes};
use super::*;
use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use terrarium_core::live_workspace::{NamespaceIdentity, RuntimeIdentity, SourceIdentity};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Cleanup(Vec<File>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for mount in self.0.iter().rev() {
            let _ = mounts::lazy_detach(mount);
        }
    }
}

fn docker(args: &[&str]) -> Vec<u8> {
    let output = Command::new("/usr/bin/docker")
        .args(["--host", "unix:///var/run/docker.sock"])
        .args(args)
        .env_clear()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "synthetic Docker fixture command failed"
    );
    output.stdout
}

struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/docker")
            .args([
                "--host",
                "unix:///var/run/docker.sock",
                "rm",
                "--force",
                &self.0,
            ])
            .env_clear()
            .output();
    }
}

#[test]
#[ignore = "requires explicit disposable Docker VM"]
fn docker_runtime_identity_and_restart() {
    use runtime::Inspector;
    assert_eq!(std::env::var("TERRARIUM_B2_DISPOSABLE").as_deref(), Ok("1"));
    assert!(nix::unistd::geteuid().is_root());
    let image = String::from_utf8(docker(&[
        "image",
        "inspect",
        "busybox:1.37.0",
        "--format",
        "{{.Id}}",
    ]))
    .unwrap();
    let image = image.trim();
    assert!(image.starts_with("sha256:") && image.len() == 71);
    let request = super::tests::envelope(1, Kind::Grant).original;
    let registration = format!(
        "terrarium.workspace.registration={}",
        request.runtime.registration.0
    );
    let incarnation = format!(
        "terrarium.workspace.runtime={}",
        request.runtime.incarnation.0
    );
    let id = String::from_utf8(docker(&[
        "run",
        "--detach",
        "--pull",
        "never",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "65534:65534",
        "--memory",
        "64m",
        "--pids-limit",
        "16",
        "--label",
        &registration,
        "--label",
        &incarnation,
        image,
        "/bin/sleep",
        "600",
    ]))
    .unwrap();
    let container = Container(id.trim().to_owned());
    assert_eq!(container.0.len(), 64);
    let mut inspector = runtime::DockerInspector;
    let snapshot = inspector.inspect(&container.0).unwrap();
    assert_eq!(snapshot.identity.registration, request.runtime.registration);
    assert_eq!(snapshot.identity.incarnation, request.runtime.incarnation);
    let pin = runtime::PinnedRuntime::acquire(&mut inspector, &snapshot.identity).unwrap();
    assert!(!pin.mountinfo(&mut inspector).unwrap().is_empty());
    assert!(inspector.inspect(&container.0[..12]).is_err());
    let mut wrong = snapshot.identity.clone();
    wrong.namespace.inode += 1;
    assert!(runtime::PinnedRuntime::acquire(&mut inspector, &wrong).is_err());
    docker(&["pause", &container.0]);
    assert!(pin.recheck(&mut inspector, &snapshot.identity).is_err());
    docker(&["unpause", &container.0]);
    docker(&["restart", "--time", "1", &container.0]);
    assert!(pin.recheck(&mut inspector, &snapshot.identity).is_err());
    let restarted = inspector.inspect(&container.0).unwrap();
    assert_ne!(snapshot.started_at, restarted.started_at);
    assert_ne!(snapshot.identity.namespace, restarted.identity.namespace);
    drop(container); // delete only the exact container created by this test
}

fn ns(path: &str) -> NamespaceIdentity {
    let m = std::fs::metadata(path).unwrap();
    NamespaceIdentity {
        device: m.dev(),
        inode: m.ino(),
    }
}
fn host_view() -> Vec<terrarium_core::live_workspace::MountEvidence> {
    mountinfo::parse(
        &std::fs::read("/proc/self/mountinfo").unwrap(),
        ns("/proc/self/ns/mnt"),
    )
    .unwrap()
}
fn entry(path: &Path) -> terrarium_core::live_workspace::MountEvidence {
    host_view()
        .into_iter()
        .find(|m| Path::new(&m.mountpoint) == path)
        .unwrap()
}
fn isolate(path: &Path, cleanup: &mut Cleanup, shared: bool) -> File {
    let underlying = File::open(path).unwrap();
    let tree = mounts::clone_nonrecursive(&underlying).unwrap();
    mounts::attach(&tree, &underlying).unwrap();
    let mounted = File::open(path).unwrap();
    cleanup.0.push(mounted.try_clone().unwrap());
    mounts::make_private(&mounted).unwrap();
    assert!(entry(path).optional_fields.is_empty());
    if shared {
        mounts::make_shared(&mounted).unwrap();
    }
    mounted
}

struct Receiver {
    snapshot: runtime::Snapshot,
}
impl runtime::Inspector for Receiver {
    fn inspect(&mut self, _: &str) -> Result<runtime::Snapshot, Reply> {
        Ok(self.snapshot.clone())
    }
}

#[test]
#[ignore = "requires explicit disposable VM and a private outer mount namespace"]
fn kernel_publish_revoke_and_refusals() {
    assert_eq!(std::env::var("TERRARIUM_B2_DISPOSABLE").as_deref(), Ok("1"));
    assert!(nix::unistd::geteuid().is_root());
    assert_ne!(
        ns("/proc/self/ns/mnt"),
        ns("/proc/1/ns/mnt"),
        "launch this fixture with unshare --mount --propagation private"
    );
    let fixture = tempfile::Builder::new()
        .prefix("ter925-fixture-")
        .tempdir_in("/root")
        .unwrap();
    let mut cleanup = Cleanup(vec![]);
    for name in ["source", "staging", "publisher", "receiver", "other"] {
        std::fs::create_dir(fixture.path().join(name)).unwrap();
    }
    let source_path = fixture.path().join("source");
    let staging_path = fixture.path().join("staging");
    let publisher_path = fixture.path().join("publisher");
    let receiver_path = fixture.path().join("receiver");
    let source = isolate(&source_path, &mut cleanup, true);
    let _staging = isolate(&staging_path, &mut cleanup, false);
    let publisher = isolate(&publisher_path, &mut cleanup, true);
    LinuxAttributes.scaffold(&publisher).unwrap();
    std::fs::create_dir(publisher_path.join("reverse")).unwrap();
    std::fs::write(
        fixture.path().join("other/reverse-canary"),
        b"receiver-only",
    )
    .unwrap();

    let ready = fixture.path().join("receiver-ready");
    // Test harness only. Fixed absolute binaries and positional synthetic paths.
    // Production helper code never shells out to mount or accepts shell text.
    let mut child = ChildGuard(Command::new("/usr/bin/unshare")
        .args(["--mount", "--propagation", "unchanged", "/bin/sh", "-c",
            "/usr/bin/mount --bind \"$1\" \"$2\" && /usr/bin/mount --make-rslave \"$2\" && /usr/bin/mount -o remount,bind,ro \"$2\" && /usr/bin/touch \"$3\" && exec /usr/bin/sleep 600", "fixture"])
        .arg(&publisher_path).arg(&receiver_path).arg(&ready).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "receiver setup failed"
        );
        assert!(Instant::now() < deadline, "receiver setup deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = child.0.id();
    let mut identity: RuntimeIdentity = super::tests::envelope(1, Kind::Grant).original.runtime;
    identity.namespace = ns(&format!("/proc/{pid}/ns/mnt"));
    let mut inspector = Receiver {
        snapshot: runtime::Snapshot {
            identity: identity.clone(),
            pid,
            started_at: "synthetic-runtime".into(),
            running: true,
        },
    };
    let pin = runtime::PinnedRuntime::acquire(&mut inspector, &identity).unwrap();
    let receiver_entry = |inspector: &mut Receiver| {
        pin.mountinfo(inspector)
            .unwrap()
            .into_iter()
            .find(|m| Path::new(&m.mountpoint) == receiver_path)
            .unwrap()
    };
    let host_publisher = entry(&publisher_path);
    mountinfo::publisher_fixture(
        &host_publisher,
        &receiver_entry(&mut inspector),
        &host_view(),
        receiver_path.to_str().unwrap(),
    )
    .unwrap();
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
        std::fs::set_permissions(
            workspace.join("run"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let mut enrollment = super::tests::envelope(1, Kind::Grant)
            .original
            .holder
            .unwrap()
            .enrollment;
        enrollment.name = name.into();
        let m = std::fs::metadata(&workspace).unwrap();
        enrollment.source = SourceIdentity {
            device: m.dev(),
            inode: m.ino(),
        };
        let opened = source_root.enrolled(&enrollment).unwrap();
        let mut wrong = enrollment.clone();
        wrong.source.inode += 1;
        assert!(source_root.enrolled(&wrong).is_err());
        wrong = enrollment.clone();
        wrong.source.device += 1;
        assert!(source_root.enrolled(&wrong).is_err());
        let renamed = source_path.join("project").join(format!("{name}-original"));
        std::fs::rename(&workspace, &renamed).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        assert!(source_root.enrolled(&enrollment).is_err());
        std::fs::remove_dir(&workspace).unwrap();
        std::os::unix::fs::symlink(&renamed, &workspace).unwrap();
        assert!(source_root.enrolled(&enrollment).is_err());
        std::fs::remove_file(&workspace).unwrap();
        std::fs::rename(&renamed, &workspace).unwrap();
        scan_source(&opened).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(workspace.join("socket")).unwrap();
        assert_eq!(scan_source(&opened), Err(Reply::Unauthorized));
        drop(socket);
        std::fs::remove_file(workspace.join("socket")).unwrap();
        std::fs::create_dir(workspace.join("nested")).unwrap();
        let nested_underlying = File::open(workspace.join("nested")).unwrap();
        let foreign =
            mounts::clone_nonrecursive(&File::open(fixture.path().join("other")).unwrap()).unwrap();
        mounts::attach(&foreign, &nested_underlying).unwrap();
        let nested = File::open(workspace.join("nested")).unwrap();
        assert_eq!(scan_source(&opened), Err(Reply::Unauthorized));
        mounts::lazy_detach(&nested).unwrap();
        scan_source(&opened).unwrap();
        let device = workspace.join("device");
        nix::sys::stat::mknod(
            &device,
            nix::sys::stat::SFlag::S_IFCHR,
            nix::sys::stat::Mode::S_IRUSR,
            nix::sys::stat::makedev(1, 3),
        )
        .unwrap();
        assert_eq!(scan_source(&opened), Err(Reply::Unauthorized));
        std::fs::remove_file(&device).unwrap();

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
        assert!(
            mountinfo::private_stage(
                &entry(&stage_path),
                &expected_root,
                &entry(&source_path).major_minor
            )
            .is_err()
        );
        mounts::make_private(&staged).unwrap();
        mountinfo::private_stage(
            &entry(&stage_path),
            &expected_root,
            &entry(&source_path).major_minor,
        )
        .unwrap();
        mounts::attach(&staged, destination.placeholder().unwrap()).unwrap();
        stage.remove().unwrap();
        let host_mount = entry(&publisher_path.join("project").join(name));
        let receiver_mount = pin
            .mountinfo(&mut inspector)
            .unwrap()
            .into_iter()
            .find(|m| Path::new(&m.mountpoint) == receiver_path.join("project").join(name))
            .unwrap();
        mountinfo::workspace(
            &host_mount,
            &receiver_mount,
            &host_publisher,
            &receiver_entry(&mut inspector),
            &expected_root,
            &entry(&source_path).major_minor,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(receiver_lookup.join("project").join(name).join("canary")).unwrap(),
            name.as_bytes()
        );
        std::fs::write(
            receiver_lookup.join("project").join(name).join("writable"),
            b"yes",
        )
        .unwrap();
        assert_eq!(
            Command::new(receiver_lookup.join("project").join(name).join("run"))
                .status()
                .unwrap()
                .code(),
            Some(17)
        );
        destinations.push(destination);
    }
    let revoked = destinations.remove(0);
    let mount = revoked.mounted().unwrap();
    if mounts::ordinary_unmount(&mount).unwrap() == mounts::Unmount::Busy {
        mounts::lazy_detach(&mount).unwrap();
    }
    revoked.remove().unwrap();
    assert_eq!(
        std::fs::metadata(receiver_lookup.join("project/agent"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        std::fs::read(receiver_lookup.join("project/agent2/canary")).unwrap(),
        b"agent2"
    );
    assert!(child.0.try_wait().unwrap().is_none());
    // Reverse propagation: a receiver-local mount must not appear on publisher.
    let status = Command::new("/usr/bin/nsenter")
        .arg(format!("--mount=/proc/{pid}/ns/mnt"))
        .args(["/usr/bin/mount", "--bind"])
        .arg(fixture.path().join("other"))
        .arg(receiver_path.join("reverse"))
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read(receiver_lookup.join("reverse/reverse-canary")).unwrap(),
        b"receiver-only"
    );
    assert!(!publisher_path.join("reverse/reverse-canary").exists());
    assert!(
        !host_view()
            .iter()
            .any(|m| Path::new(&m.mountpoint) == publisher_path.join("reverse"))
    );
    assert!(
        Command::new("/usr/bin/nsenter")
            .arg(format!("--mount=/proc/{pid}/ns/mnt"))
            .arg("/usr/bin/umount")
            .arg(receiver_path.join("reverse"))
            .status()
            .unwrap()
            .success()
    );
    // Exercise the durable service/backend path after the raw primitive proof.
    let remaining = destinations.pop().unwrap();
    let remaining_mount = remaining.mounted().unwrap();
    if mounts::ordinary_unmount(&remaining_mount).unwrap() == mounts::Unmount::Busy {
        mounts::lazy_detach(&remaining_mount).unwrap();
    }
    remaining.remove().unwrap();
    let state_path = fixture.path().join("state");
    std::fs::create_dir(&state_path).unwrap();
    let store = Store::open(ProtectedRoot::open(&state_path).unwrap()).unwrap();
    let mut journal = super::tests::journal();
    journal.runtime = identity.clone();
    store.save(&journal).unwrap();
    let kernel = || {
        let mut docker = Receiver {
            snapshot: inspector.snapshot.clone(),
        };
        let pin = runtime::PinnedRuntime::acquire(&mut docker, &identity).unwrap();
        kernel::Kernel::new(
            ProtectedRoot::open(&source_path).unwrap(),
            ProtectedRoot::open(&publisher_path).unwrap(),
            ProtectedRoot::open(&staging_path).unwrap(),
            pin,
            docker,
            LinuxAttributes,
        )
        .unwrap()
        .fixture_receiver(&receiver_path)
    };
    // Foreign receiver mounts quarantine even when the source/root/flags of
    // the legitimate publisher are otherwise correct. Nothing is deleted.
    assert!(
        Command::new("/usr/bin/nsenter")
            .arg(format!("--mount=/proc/{pid}/ns/mnt"))
            .args(["/usr/bin/mount", "--bind"])
            .arg(fixture.path().join("other"))
            .arg(receiver_path.join("reverse"))
            .status()
            .unwrap()
            .success()
    );
    let foreign_state = fixture.path().join("foreign-state");
    std::fs::create_dir(&foreign_state).unwrap();
    let foreign_store = Store::open(ProtectedRoot::open(&foreign_state).unwrap()).unwrap();
    foreign_store.save(&journal).unwrap();
    let foreign_service = Helper::open(foreign_store, kernel()).unwrap();
    assert!(foreign_service.journal().quarantined);
    assert!(receiver_lookup.join("reverse/reverse-canary").exists());
    drop(foreign_service);
    assert!(
        Command::new("/usr/bin/nsenter")
            .arg(format!("--mount=/proc/{pid}/ns/mnt"))
            .arg("/usr/bin/umount")
            .arg(receiver_path.join("reverse"))
            .status()
            .unwrap()
            .success()
    );
    let request = |revision, kind, name: &str| {
        let mut e = super::tests::envelope(revision, kind);
        e.original.runtime = identity.clone();
        let holder = e.original.holder.as_mut().unwrap();
        holder.enrollment.name = name.into();
        let source = std::fs::metadata(source_path.join("project").join(name)).unwrap();
        holder.enrollment.source = SourceIdentity {
            device: source.dev(),
            inode: source.ino(),
        };
        if name == "agent2" {
            holder.attachment = serde_json::from_str("\"00000000000000000000000009\"").unwrap();
            holder.enrollment.workspace =
                serde_json::from_str("\"00000000000000000000000009\"").unwrap();
        }
        e.original_hash = e.original.hash().unwrap();
        serde_json::to_vec(&e).unwrap()
    };
    let mut service = Helper::open(store, kernel()).unwrap();
    assert!(!service.journal().quarantined);
    let first = request(1, Kind::Grant, "agent");
    assert_eq!(service.apply(&first), Ok(Reply::Applied));
    assert_eq!(service.apply(&first), Ok(Reply::Applied));
    assert_eq!(
        service.apply(&request(2, Kind::Grant, "agent2")),
        Ok(Reply::Applied)
    );
    let retained = File::open(receiver_lookup.join("project/agent")).unwrap();
    drop(service);
    let mut service = Helper::open(
        Store::open(ProtectedRoot::open(&state_path).unwrap()).unwrap(),
        kernel(),
    )
    .unwrap();
    assert!(
        !service.journal().quarantined,
        "committed restart must reconcile both views"
    );
    assert_eq!(
        service.apply(&request(3, Kind::Revoke, "agent")),
        Ok(Reply::PathRevokedVerified)
    );
    assert!(!receiver_lookup.join("project/agent").exists());
    std::fs::write(
        source_path.join("project/agent/late"),
        b"retained-handle-limit",
    )
    .unwrap();
    let mut late = File::from(
        nix::fcntl::openat(
            &retained,
            "late",
            nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .unwrap(),
    );
    let mut value = String::new();
    std::io::Read::read_to_string(&mut late, &mut value).unwrap();
    assert_eq!(value, "retained-handle-limit");
    assert!(receiver_lookup.join("project/agent2/canary").exists());
    assert_eq!(
        service.apply(&request(4, Kind::Revoke, "agent2")),
        Ok(Reply::PathRevokedVerified)
    );
    assert_eq!(service.store.load().unwrap().last_contiguous_revision, 4);
    assert!(child.0.try_wait().unwrap().is_none());
    assert_eq!(
        mount_id(&source).unwrap(),
        mount_id(&source_root.fd).unwrap()
    );
    drop(child); // stop only the exact child started by this fixture
    drop(cleanup); // detach only fixture-owned pinned mounts before tmp cleanup
}

struct InterruptedFixture {
    receiver: ChildGuard,
    cleanup: Cleanup,
    root: tempfile::TempDir,
    snapshot: runtime::Snapshot,
}

impl InterruptedFixture {
    fn create() -> Self {
        assert_eq!(std::env::var("TERRARIUM_B2_DISPOSABLE").as_deref(), Ok("1"));
        assert!(nix::unistd::geteuid().is_root());
        assert_ne!(ns("/proc/self/ns/mnt"), ns("/proc/1/ns/mnt"));
        let root = tempfile::Builder::new()
            .prefix("ter925-crash-")
            .tempdir_in("/root")
            .unwrap();
        for name in [
            "source",
            "staging",
            "publisher",
            "receiver",
            "state",
            "config",
            "socket",
        ] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let mut cleanup = Cleanup(vec![]);
        isolate(&root.path().join("source"), &mut cleanup, true);
        isolate(&root.path().join("staging"), &mut cleanup, false);
        let publisher = isolate(&root.path().join("publisher"), &mut cleanup, true);
        LinuxAttributes.scaffold(&publisher).unwrap();
        std::fs::create_dir(root.path().join("source/project")).unwrap();
        for name in ["agent", "agent2"] {
            let workspace = root.path().join("source/project").join(name);
            std::fs::create_dir(&workspace).unwrap();
            std::fs::write(workspace.join("canary"), name.as_bytes()).unwrap();
        }
        let ready = root.path().join("ready");
        let mut receiver = ChildGuard(Command::new("/usr/bin/unshare").args(["--mount", "--propagation", "unchanged", "/bin/sh", "-c", "/usr/bin/mount --bind \"$1\" \"$2\" && /usr/bin/mount --make-rslave \"$2\" && /usr/bin/mount -o remount,bind,ro \"$2\" && /usr/bin/touch \"$3\" && exec /usr/bin/sleep 600", "fixture"])
            .arg(root.path().join("publisher")).arg(root.path().join("receiver")).arg(&ready).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(receiver.0.try_wait().unwrap().is_none());
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = receiver.0.id();
        let mut identity = super::tests::envelope(1, Kind::Grant).original.runtime;
        identity.namespace = ns(&format!("/proc/{pid}/ns/mnt"));
        let snapshot = runtime::Snapshot {
            identity,
            pid,
            started_at: "interrupted-fixture".into(),
            running: true,
        };
        let fixture = Self {
            receiver,
            cleanup,
            root,
            snapshot,
        };
        fixture.preflight();
        let store =
            Store::open(ProtectedRoot::open(&fixture.root.path().join("state")).unwrap()).unwrap();
        let mut journal = super::tests::journal();
        journal.runtime = fixture.snapshot.identity.clone();
        store.save(&journal).unwrap();
        fixture
    }

    fn preflight(&self) {
        preflight_fixture(self.root.path());
    }

    fn kernel(&self) -> kernel::Kernel<Receiver, LinuxAttributes> {
        fixture_kernel(self.root.path(), self.snapshot.clone())
    }

    fn helper(&self) -> Helper<kernel::Kernel<Receiver, LinuxAttributes>> {
        self.preflight();
        Helper::open(
            Store::open(ProtectedRoot::open(&self.root.path().join("state")).unwrap()).unwrap(),
            self.kernel(),
        )
        .unwrap()
    }

    fn request(&self, revision: u64, kind: Kind, name: &str) -> Vec<u8> {
        fixture_request(
            self.root.path(),
            &self.snapshot.identity,
            revision,
            kind,
            name,
        )
    }

    fn lookup(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/proc/{}/root", self.snapshot.pid))
            .join(self.root.path().strip_prefix("/").unwrap())
            .join("receiver")
    }

    fn tables(
        &self,
    ) -> (
        Vec<terrarium_core::live_workspace::MountEvidence>,
        Vec<terrarium_core::live_workspace::MountEvidence>,
    ) {
        let host = host_view()
            .into_iter()
            .filter(|m| Path::new(&m.mountpoint).starts_with(self.root.path()))
            .collect();
        let mut inspector = Receiver {
            snapshot: self.snapshot.clone(),
        };
        let pin = runtime::PinnedRuntime::acquire(&mut inspector, &self.snapshot.identity).unwrap();
        let receiver = pin
            .mountinfo(&mut inspector)
            .unwrap()
            .into_iter()
            .filter(|m| Path::new(&m.mountpoint).starts_with(self.root.path()))
            .collect();
        (host, receiver)
    }
}

fn preflight_fixture(root: &Path) {
    let data: Vec<_> = ["source", "publisher", "staging"]
        .iter()
        .map(|p| ProtectedRoot::open(&root.join(p)).unwrap())
        .collect();
    let controls: Vec<_> = ["state", "config", "socket"]
        .iter()
        .map(|p| ProtectedRoot::open(&root.join(p)).unwrap())
        .collect();
    confinement::validate(
        &data.iter().collect::<Vec<_>>(),
        &controls.iter().collect::<Vec<_>>(),
    )
    .unwrap();
}

fn fixture_kernel(
    root: &Path,
    snapshot: runtime::Snapshot,
) -> kernel::Kernel<Receiver, LinuxAttributes> {
    let mut inspector = Receiver { snapshot };
    let identity = inspector.snapshot.identity.clone();
    let pin = runtime::PinnedRuntime::acquire(&mut inspector, &identity).unwrap();
    kernel::Kernel::new(
        ProtectedRoot::open(&root.join("source")).unwrap(),
        ProtectedRoot::open(&root.join("publisher")).unwrap(),
        ProtectedRoot::open(&root.join("staging")).unwrap(),
        pin,
        inspector,
        LinuxAttributes,
    )
    .unwrap()
    .fixture_receiver(&root.join("receiver"))
}

fn fixture_request(
    root: &Path,
    identity: &RuntimeIdentity,
    revision: u64,
    kind: Kind,
    name: &str,
) -> Vec<u8> {
    let mut e = super::tests::envelope(revision, kind);
    e.original.runtime = identity.clone();
    let holder = e.original.holder.as_mut().unwrap();
    holder.enrollment.name = name.into();
    let m = std::fs::metadata(root.join("source/project").join(name)).unwrap();
    holder.enrollment.source = SourceIdentity {
        device: m.dev(),
        inode: m.ino(),
    };
    if name == "agent2" {
        holder.attachment = serde_json::from_str("\"00000000000000000000000009\"").unwrap();
        holder.enrollment.workspace =
            serde_json::from_str("\"00000000000000000000000009\"").unwrap();
    }
    e.original_hash = e.original.hash().unwrap();
    serde_json::to_vec(&e).unwrap()
}

/// The probe is a separate process in the parent's private mount namespace.
/// SIGABRT closes all its FDs/locks but does not undo its actual mount changes.
pub(super) fn run_crash_child() {
    assert_eq!(std::env::var("TERRARIUM_B2_DISPOSABLE").as_deref(), Ok("1"));
    assert!(nix::unistd::geteuid().is_root());
    let root = std::path::PathBuf::from(std::env::var_os("TERRARIUM_B2_CASE_ROOT").unwrap());
    let pid: u32 = std::env::var("TERRARIUM_B2_RECEIVER_PID")
        .unwrap()
        .parse()
        .unwrap();
    let mut identity = super::tests::envelope(1, Kind::Grant).original.runtime;
    identity.namespace = ns(&format!("/proc/{pid}/ns/mnt"));
    let snapshot = runtime::Snapshot {
        identity: identity.clone(),
        pid,
        started_at: "interrupted-fixture".into(),
        running: true,
    };
    let revoke = std::env::var("TERRARIUM_B2_REVOKE").as_deref() == Ok("1");
    preflight_fixture(&root);
    let mut helper = Helper::open(
        Store::open(ProtectedRoot::open(&root.join("state")).unwrap()).unwrap(),
        fixture_kernel(&root, snapshot),
    )
    .unwrap();
    assert!(!helper.journal().quarantined);
    let request = fixture_request(
        &root,
        &identity,
        if revoke { 3 } else { 2 },
        if revoke { Kind::Revoke } else { Kind::Grant },
        "agent",
    );
    let result = helper.apply(&request);
    panic!("expected crash hook did not fire: {result:?}");
}

#[test]
#[ignore = "requires explicit disposable VM and private outer mount namespace"]
fn interrupted_syscalls_reopen_without_replay_or_resurrection() {
    use std::os::unix::process::ExitStatusExt;
    // Every uncommitted point is immediately after a real successful syscall
    // and before the next journal record. committed covers lost grant/revoke ack.
    for (point, revoke) in [
        ("stage_move", false),
        ("private", false),
        ("attributes", false),
        ("publish_move", false),
        ("ordinary_unmount", true),
        ("lazy_unmount", true),
        ("placeholder_remove", true),
        ("committed", false),
        ("committed", true),
    ] {
        let mut fixture = InterruptedFixture::create();
        let mut helper = fixture.helper();
        assert_eq!(
            helper.apply(&fixture.request(1, Kind::Grant, "agent2")),
            Ok(Reply::Applied)
        );
        if revoke {
            assert_eq!(
                helper.apply(&fixture.request(2, Kind::Grant, "agent")),
                Ok(Reply::Applied)
            );
        }
        drop(helper);
        let held = (point == "lazy_unmount")
            .then(|| File::open(fixture.root.path().join("publisher/project/agent")).unwrap());
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "workspace_helper::crash_probe::run"])
            .env("TERRARIUM_B2_CASE_ROOT", fixture.root.path())
            .env(
                "TERRARIUM_B2_RECEIVER_PID",
                fixture.snapshot.pid.to_string(),
            )
            .env("TERRARIUM_B2_CRASH_POINT", point)
            .env("TERRARIUM_B2_REVOKE", if revoke { "1" } else { "0" })
            .output()
            .unwrap();
        assert_eq!(
            output.status.signal(),
            Some(6),
            "{point} revoke={revoke}: child did not abort: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let before = fixture.tables();
        let mut reopened = fixture.helper();
        let revision = if revoke { 3 } else { 2 };
        let request = fixture.request(
            revision,
            if revoke { Kind::Revoke } else { Kind::Grant },
            "agent",
        );
        if point == "committed" {
            assert!(!reopened.journal().quarantined);
            assert_eq!(
                reopened.apply(&request),
                Ok(if revoke {
                    Reply::PathRevokedVerified
                } else {
                    Reply::Applied
                })
            );
            assert_eq!(reopened.journal().last_contiguous_revision, revision);
        } else {
            assert!(reopened.journal().quarantined, "{point}");
            assert_eq!(reopened.apply(&request), Err(Reply::Quarantined));
        }
        assert_eq!(
            before,
            fixture.tables(),
            "{point}: startup/retry changed unknown kernel state"
        );
        assert_eq!(
            std::fs::read(fixture.lookup().join("project/agent2/canary")).unwrap(),
            b"agent2"
        );
        assert!(fixture.receiver.0.try_wait().unwrap().is_none());
        let exposed = fixture.lookup().join("project/agent/canary");
        if revoke || matches!(point, "stage_move" | "private" | "attributes") {
            assert!(
                !exposed.exists(),
                "{point}: fresh lookup resurrected or exposed target"
            );
        } else {
            assert_eq!(std::fs::read(exposed).unwrap(), b"agent");
        }
        if point == "placeholder_remove" || (point == "committed" && revoke) {
            assert!(!fixture.lookup().join("project/agent").exists());
        }
        if let Some(fd) = held {
            let file = File::from(
                nix::fcntl::openat(
                    &fd,
                    "canary",
                    nix::fcntl::OFlag::O_RDONLY,
                    nix::sys::stat::Mode::empty(),
                )
                .unwrap(),
            );
            assert_eq!(file.metadata().unwrap().len(), 5); // retained handle, not resurrection
        }
        drop(reopened);
        // Read the field so clippy also verifies the fixture intentionally owns
        // cleanup guards until after all interrupted-state assertions.
        assert_eq!(fixture.cleanup.0.len(), 3);
    }
}

#[test]
#[ignore = "requires explicit disposable VM and private outer mount namespace"]
fn control_state_aliases_refused_before_receiver_export() {
    assert_eq!(std::env::var("TERRARIUM_B2_DISPOSABLE").as_deref(), Ok("1"));
    assert!(nix::unistd::geteuid().is_root());
    assert_ne!(ns("/proc/self/ns/mnt"), ns("/proc/1/ns/mnt"));
    for case in [
        "equal",
        "below",
        "bind_subtree_alias",
        "bound_into_workspace",
    ] {
        let root = tempfile::Builder::new()
            .prefix("ter925-confinement-")
            .tempdir_in("/root")
            .unwrap();
        let mut cleanup = Cleanup(vec![]);
        for name in [
            "source",
            "publisher",
            "publisher/state",
            "staging",
            "state",
            "alias",
            "receiver",
            "config",
            "socket",
            "source/project",
            "source/project/agent",
            "source/project/agent/control",
        ] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let state_path = match case {
            "equal" => root.path().join("publisher"),
            "below" => root.path().join("publisher/state"),
            "bind_subtree_alias" => {
                let source = File::open(root.path().join("publisher/state")).unwrap();
                let slot = File::open(root.path().join("alias")).unwrap();
                mounts::attach(&mounts::clone_nonrecursive(&source).unwrap(), &slot).unwrap();
                let alias = File::open(root.path().join("alias")).unwrap();
                mounts::make_private(&alias).unwrap();
                cleanup.0.push(alias);
                root.path().join("alias")
            }
            _ => root.path().join("state"),
        };
        let state = ProtectedRoot::open(&state_path).unwrap();
        let store = Store::open(state.try_clone().unwrap()).unwrap();
        let mut journal = super::tests::journal();
        journal.target = "journal-canary".into();
        store.save(&journal).unwrap();
        drop(store);
        let canary = std::fs::read(state_path.join("journal.json")).unwrap();
        if case == "bound_into_workspace" {
            let slot = File::open(root.path().join("source/project/agent/control")).unwrap();
            mounts::attach(&mounts::clone_nonrecursive(&state.fd).unwrap(), &slot).unwrap();
            let alias = File::open(root.path().join("source/project/agent/control")).unwrap();
            mounts::make_private(&alias).unwrap();
            cleanup.0.push(alias);
        }
        let data: Vec<_> = ["source", "publisher", "staging"]
            .iter()
            .map(|p| ProtectedRoot::open(&root.path().join(p)).unwrap())
            .collect();
        let config = ProtectedRoot::open(&root.path().join("config")).unwrap();
        let socket = ProtectedRoot::open(&root.path().join("socket")).unwrap();
        let admission = confinement::validate(
            &data.iter().collect::<Vec<_>>(),
            &[&state, &config, &socket],
        );
        // Model the export gate: no receiver construction or store mutation is
        // allowed until this exact read-only startup preflight has succeeded.
        if admission.is_ok() {
            let source = File::open(if case == "bound_into_workspace" {
                root.path().join("source/project/agent")
            } else {
                root.path().join("publisher")
            })
            .unwrap();
            let slot = File::open(root.path().join("receiver")).unwrap();
            mounts::attach(&mounts::clone_nonrecursive(&source).unwrap(), &slot).unwrap();
            cleanup
                .0
                .push(File::open(root.path().join("receiver")).unwrap());
        }
        assert_eq!(admission, Err(Reply::Unauthorized), "{case}");
        assert!(!root.path().join("receiver/journal.json").exists());
        assert!(!root.path().join("receiver/state/journal.json").exists());
        assert!(!root.path().join("receiver/control/journal.json").exists());
        assert_eq!(
            std::fs::read(state_path.join("journal.json")).unwrap(),
            canary
        );
        assert!(!state_path.join("journal.next").exists());
    }
}
