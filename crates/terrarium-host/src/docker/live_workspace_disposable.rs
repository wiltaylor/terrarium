//! Synthetic B3 Docker receiver proof. No daemon registration or helper service.
use super::*;
use crate::workspace_helper::{mounts, runtime};
use crate::workspace_helper::mounts::{Attributes, LinuxAttributes};
use crate::workspace_helper::runtime::Inspector;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use terrarium_core::live_workspace::NamespaceIdentity;

fn docker(args: &[String]) -> Vec<u8> {
    let result = Command::new("/usr/bin/docker").args(["--host", "unix:///var/run/docker.sock"])
        .args(args).env_clear().output().unwrap();
    assert!(result.status.success(), "synthetic Docker command: {}", String::from_utf8_lossy(&result.stderr));
    result.stdout
}
fn cmd(args: &[&str]) -> Vec<u8> { docker(&args.iter().map(|s| (*s).into()).collect::<Vec<_>>()) }
struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/docker").args(["--host", "unix:///var/run/docker.sock", "rm", "--force", &self.0]).env_clear().output();
    }
}
struct Mounts(Vec<File>);
impl Drop for Mounts {
    fn drop(&mut self) { for fd in self.0.iter().rev() { let _ = mounts::lazy_detach(fd); } }
}
fn host() -> Vec<MountEvidence> {
    let ns = std::fs::metadata("/proc/self/ns/mnt").unwrap();
    mountinfo::parse(&std::fs::read("/proc/self/mountinfo").unwrap(), NamespaceIdentity {device:ns.dev(),inode:ns.ino()}).unwrap()
}

#[test]
#[ignore = "requires explicit disposable Docker VM, in the Docker daemon mount namespace"]
fn docker_readonly_scaffold_later_publish_and_revoke() {
    assert_eq!(std::env::var("TERRARIUM_B3_DISPOSABLE").as_deref(), Ok("1"));
    assert!(nix::unistd::geteuid().is_root());
    // Unlike B2's isolated synthetic receiver, Docker needs the publisher in
    // its own host namespace. All mountpoints below belong to this tempdir.
    let root = tempfile::Builder::new().prefix("ter926-").tempdir_in("/root").unwrap();
    let mut cleanup = Mounts(vec![]);
    let publisher_path = root.path().join("publisher");
    std::fs::create_dir(&publisher_path).unwrap();
    let original = File::open(&publisher_path).unwrap();
    let tree = mounts::clone_nonrecursive(&original).unwrap();
    mounts::attach(&tree, &original).unwrap();
    let publisher = File::open(&publisher_path).unwrap();
    cleanup.0.push(publisher.try_clone().unwrap());
    mounts::make_private(&publisher).unwrap();
    LinuxAttributes.scaffold(&publisher).unwrap();
    mounts::make_shared(&publisher).unwrap();

    let (mut r, mut req, _) = super::tests::fixture();
    req.name = format!("ter926-{}", std::process::id());
    req.image = String::from_utf8(cmd(&["image", "inspect", "busybox:1.37.0", "--format", "{{.Id}}"])).unwrap().trim().into();
    req.command = vec!["/bin/sleep".into(), "600".into()];
    let mut args = create_flags(&req, &r, &r.profile).unwrap();
    // Only this test substitutes a tempdir for the fixed administrative path.
    let index = args.iter().position(|v| v.starts_with("type=bind,")).unwrap();
    args[index] = format!("type=bind,src={},dst=/work,readonly,bind-propagation=rslave", publisher_path.display());
    let id = String::from_utf8(docker(&args)).unwrap().trim().to_owned();
    assert_eq!(id.len(), 64);
    let container = Container(id);
    cmd(&["start", &container.0]);
    let mut inspector = runtime::DockerInspector;
    let snapshot = inspector.inspect(&container.0).unwrap();
    r.runtime = snapshot.identity.clone();
    r.publisher = publisher_path.to_str().unwrap().into();
    let pin = runtime::PinnedRuntime::acquire(&mut inspector, &r.runtime).unwrap();
    let initial = pin.mountinfo(&mut inspector).unwrap();
    let evidence = verify_empty_scaffold(&r, &host(), &initial).unwrap();
    let inspect = String::from_utf8(cmd(&["inspect", &container.0])).unwrap();
    parse_verified_inspect(&inspect, &r, &r.profile, Some(&evidence)).unwrap();
    cmd(&["exec", &container.0, "sh", "-c", "test ! -e /work/project && ! touch /work/forbidden && echo sentinel >/tmp/ter926-sentinel"]);

    // Publish only synthetic directories, with the same nonrecursive B2
    // clone/private/restrict/move operations, after Docker's ro receiver exists.
    std::fs::create_dir(publisher_path.join("project")).unwrap();
    for name in ["first", "later"] {
        let source = root.path().join(name);
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("canary"), name).unwrap();
        let destination = publisher_path.join("project").join(name);
        std::fs::create_dir(&destination).unwrap();
        let tree = mounts::clone_nonrecursive(&File::open(&source).unwrap()).unwrap();
        let stage_path = root.path().join(format!("stage-{name}"));
        std::fs::create_dir(&stage_path).unwrap();
        mounts::attach(&tree, &File::open(&stage_path).unwrap()).unwrap();
        let stage = File::open(&stage_path).unwrap();
        cleanup.0.push(stage.try_clone().unwrap());
        mounts::make_private(&stage).unwrap();
        LinuxAttributes.workspace(&stage).unwrap();
        mounts::attach(&stage, &File::open(&destination).unwrap()).unwrap();
        cleanup.0.pop();
        std::fs::remove_dir(&stage_path).unwrap();
        cleanup.0.push(File::open(&destination).unwrap());
        assert_eq!(cmd(&["exec", &container.0, "cat", &format!("/work/project/{name}/canary")]), name.as_bytes());
        cmd(&["exec", &container.0, "sh", "-c", &format!("echo writable >/work/project/{name}/written")]);
        assert!(source.join("written").exists());
        pin.recheck(&mut inspector, &r.runtime).unwrap();
    }
    let h = host(); let c = pin.mountinfo(&mut inspector).unwrap();
    let hp = h.iter().find(|m| m.mountpoint == r.publisher).unwrap();
    let cp = c.iter().find(|m| m.mountpoint == "/work").unwrap();
    mountinfo::publisher(hp, cp, &h).unwrap();
    for name in ["first", "later"] {
        let hm = h.iter().find(|m| m.mountpoint == format!("{}/project/{name}", r.publisher)).unwrap();
        let cm = c.iter().find(|m| m.mountpoint == format!("/work/project/{name}")).unwrap();
        mountinfo::workspace(hm, cm, hp, cp, &hm.root, &hm.major_minor).unwrap();
    }
    let first = cleanup.0.pop().unwrap(); // later remains present during first revoke below
    let earlier = cleanup.0.pop().unwrap();
    if mounts::ordinary_unmount(&earlier).unwrap() == mounts::Unmount::Busy { mounts::lazy_detach(&earlier).unwrap(); }
    std::fs::remove_dir(publisher_path.join("project/first")).unwrap();
    cmd(&["exec", &container.0, "sh", "-c", "test ! -e /work/project/first && test -f /work/project/later/canary && test -f /tmp/ter926-sentinel && ! mount -t tmpfs tmpfs /work"]);
    assert!(!pin.mountinfo(&mut inspector).unwrap().iter().any(|m| m.mountpoint == "/work/project/first"));
    pin.recheck(&mut inspector, &r.runtime).unwrap();
    cleanup.0.push(first);
    drop(container);
    drop(cleanup);
}
