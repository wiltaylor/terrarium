use super::*;
use std::cell::RefCell;
use std::rc::Rc;
use terrarium_core::live_workspace::*;

fn id<T: serde::de::DeserializeOwned>(n: u64) -> T {
    serde_json::from_str(&format!("\"{n:026}\"")).unwrap()
}

fn envelope(revision: u64, kind: Kind) -> Envelope {
    let request = Request {
        version: PROTOCOL_VERSION,
        provenance: Provenance { daemon_instance: id(1), caller: Caller::LocalOperator },
        operation: id(revision), host: id(2), target: "fixture".into(),
        runtime: RuntimeIdentity { registration: id(3), incarnation: id(4), container_id: "a".repeat(64), namespace: NamespaceIdentity { device: 1, inode: 2 } },
        revision, kind,
        holder: Some(Holder { enrollment: Enrollment { workspace: id(5), host: id(2), agent: id(6), project: "project".into(), name: "agent".into(), source: SourceIdentity { device: 1, inode: 3 } }, attachment: id(7), domain: id(8) }),
    };
    Envelope { original_hash: request.hash().unwrap(), original: request, delivery: Delivery::Apply }
}

fn journal() -> Journal {
    let r = envelope(1, Kind::Grant).original;
    Journal { state_version: STATE_VERSION, host: r.host, target: r.target, runtime: r.runtime, last_contiguous_revision: 0, quarantined: false, operations: Default::default(), tombstones: Default::default() }
}

#[derive(Default)]
struct Fake { calls: Rc<RefCell<Vec<&'static str>>>, fail: Option<&'static str>, busy: bool }
impl Fake {
    fn step(&self, name: &'static str) -> Result<(), Reply> {
        self.calls.borrow_mut().push(name);
        if self.fail == Some(name) { Err(Reply::Failed) } else { Ok(()) }
    }
}
impl Backend for Fake {
    fn reconcile(&mut self, _: &Journal) -> Result<(), Reply> { self.step("reconcile") }
    fn check_runtime(&mut self, _: &Envelope) -> Result<(), Reply> { self.step("runtime") }
    fn stage(&mut self, _: &Envelope) -> Result<(), Reply> { self.step("stage") }
    fn make_private_and_restrict(&mut self, _: &Envelope) -> Result<(), Reply> { self.step("private") }
    fn publish(&mut self, _: &Envelope) -> Result<(), Reply> { self.step("publish") }
    fn unmount(&mut self, _: &Envelope) -> Result<bool, Reply> { self.step("unmount")?; Ok(self.busy) }
    fn detach(&mut self, _: &Envelope) -> Result<(), Reply> { self.step("detach") }
    fn remove_placeholder(&mut self, _: &Envelope) -> Result<(), Reply> { self.step("remove") }
    fn verify(&mut self, _: &Envelope) -> Result<(Vec<MountEvidence>, Vec<MountEvidence>), Reply> { self.step("verify")?; Ok((vec![], vec![])) }
}

fn helper(fake: Fake) -> (tempfile::TempDir, Helper<Fake>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::fixture(dir.path()).unwrap();
    store.save(&journal()).unwrap();
    (dir, Helper::open(store, fake).unwrap())
}
fn bytes(e: &Envelope) -> Vec<u8> { serde_json::to_vec(e).unwrap() }

#[test]
fn durable_order_and_exact_retry() {
    let (_dir, mut h) = helper(Fake::default());
    let e = bytes(&envelope(1, Kind::Grant));
    assert_eq!(h.apply(&e), Ok(Reply::Applied));
    assert_eq!(*h.backend.calls.borrow(), vec!["reconcile", "runtime", "stage", "private", "publish", "verify"]);
    assert_eq!(h.store.load().unwrap(), h.journal);
    let calls = h.backend.calls.borrow().len();
    assert_eq!(h.apply(&e), Ok(Reply::Applied));
    assert_eq!(h.backend.calls.borrow().len(), calls);
    assert_eq!(h.apply(&bytes(&envelope(3, Kind::Grant))), Err(Reply::RevisionGap { expected: 2 }));
    assert_eq!(h.backend.calls.borrow().len(), calls);
}

#[test]
fn every_kernel_failure_quarantines_and_never_replays() {
    for failure in ["runtime", "stage", "private", "publish", "verify", "unmount", "detach", "remove"] {
        let (dir, mut h) = helper(Fake::default());
        let e = if matches!(failure, "unmount" | "detach" | "remove") {
            h.apply(&bytes(&envelope(1, Kind::Grant))).unwrap();
            envelope(2, Kind::Revoke)
        } else { envelope(1, Kind::Grant) };
        h.backend.fail = Some(failure);
        h.backend.busy = true;
        assert_eq!(h.apply(&bytes(&e)), Err(Reply::Failed), "{failure}");
        assert!(h.store.load().unwrap().quarantined);
        drop(h);
        let mut reopened = Helper::open(Store::fixture(dir.path()).unwrap(), Fake::default()).unwrap();
        assert_eq!(reopened.apply(&bytes(&e)), Err(Reply::Quarantined));
        assert_eq!(*reopened.backend.calls.borrow(), vec!["reconcile"]);
    }
}

#[test]
fn superseded_grant_and_revoke_tombstone() {
    let (_dir, mut h) = helper(Fake::default());
    let mut e = envelope(1, Kind::Grant);
    e.delivery = Delivery::Superseded;
    assert_eq!(h.apply(&bytes(&e)), Ok(Reply::Superseded));
    assert_eq!(*h.backend.calls.borrow(), vec!["reconcile", "runtime"]);
    assert_eq!(h.apply(&bytes(&envelope(2, Kind::Revoke))), Ok(Reply::PathRevokedVerified));
    assert_eq!(h.apply(&bytes(&envelope(3, Kind::Grant))), Err(Reply::Stale));
    assert_eq!(h.store.load().unwrap().tombstones.get(&id(7)), Some(&2));
}

#[test]
fn startup_refuses_incomplete_history_and_failed_reconcile() {
    let (dir, mut h) = helper(Fake::default());
    h.journal.prepare(envelope(1, Kind::Grant)).unwrap();
    h.store.save(&h.journal).unwrap();
    drop(h);
    let h = Helper::open(Store::fixture(dir.path()).unwrap(), Fake::default()).unwrap();
    assert!(h.journal.quarantined);
    let (_dir, h) = helper(Fake { fail: Some("reconcile"), ..Fake::default() });
    assert!(h.journal.quarantined);
}

#[test]
fn store_lock_symlink_hardlink_and_interrupted_snapshot_refused() {
    use std::os::unix::fs::symlink;
    let (dir, h) = helper(Fake::default());
    assert!(matches!(Store::fixture(dir.path()), Err(Reply::Busy)));
    std::fs::hard_link(dir.path().join("journal.json"), dir.path().join("alias")).unwrap();
    assert_eq!(h.store.load(), Err(Reply::Unauthorized));
    std::fs::remove_file(dir.path().join("alias")).unwrap();
    std::fs::remove_file(dir.path().join("journal.json")).unwrap();
    symlink("canary", dir.path().join("journal.json")).unwrap();
    assert!(h.store.load().is_err());
    drop(h);
    std::fs::write(dir.path().join("journal.next"), b"incomplete").unwrap();
    assert!(matches!(Store::fixture(dir.path()), Err(Reply::Quarantined)));
}

#[test]
fn malformed_unknown_and_oversized_frames_do_not_change_state() {
    let (_dir, mut h) = helper(Fake::default());
    let before = h.journal.clone();
    for b in [b"{}".to_vec(), vec![b' '; MAX_FRAME_BYTES + 1], b"{\"unknown\":1}".to_vec()] {
        assert!(h.apply(&b).is_err());
        assert_eq!(h.journal, before);
    }
    let mut e = serde_json::to_value(envelope(1, Kind::Grant)).unwrap();
    e["original"]["namespace_pid"] = 1.into();
    assert!(h.apply(&serde_json::to_vec(&e).unwrap()).is_err());
    assert_eq!(h.journal, before);
}

#[tokio::test]
async fn peer_uid_and_frame_length_refusals() {
    let (_dir, mut h) = helper(Fake::default());
    let before = h.journal.clone();
    for wrong_uid in [true, false] {
        let (server, mut client) = UnixStream::pair().unwrap();
        client.write_all(&((MAX_FRAME_BYTES + 1) as u32).to_be_bytes()).await.unwrap();
        let uid = nix::unistd::geteuid().as_raw();
        serve_connection(&mut h, server, if wrong_uid { uid.wrapping_add(1) } else { uid }).await.unwrap();
        let size = client.read_u32().await.unwrap();
        let mut response = vec![0; size as usize];
        client.read_exact(&mut response).await.unwrap();
        let reply: Reply = serde_json::from_slice(&response).unwrap();
        assert_eq!(reply, if wrong_uid { Reply::Unauthorized } else { Reply::Failed });
        assert_eq!(h.journal, before);
    }
}

#[test]
fn protected_root_refuses_symlinks_and_writable_ancestors() {
    assert!(ProtectedRoot::open(std::path::Path::new("relative")).is_err());
    assert!(ProtectedRoot::open(std::path::Path::new("/tmp")).is_err());
    assert!(ProtectedRoot::open(std::path::Path::new("/proc/self/fd/0")).is_err());
}

fn evidence(line: &str, inode: u64) -> MountEvidence {
    mountinfo::parse(line.as_bytes(), NamespaceIdentity { device: 1, inode }).unwrap().remove(0)
}

#[test]
fn namespace_scoped_publisher_and_private_stage_verification() {
    let host = evidence("10 1 8:1 /reg/work /publisher ro - ext4 /dev/a rw", 1);
    let mut host_shared = host.clone(); host_shared.optional_fields = vec!["shared:42".into()];
    // Equal mount IDs across namespaces are neither proof nor a mismatch.
    let receiver = evidence("10 1 8:1 /reg/work /work ro master:42 - ext4 /dev/a rw", 2);
    assert_eq!(mountinfo::publisher(&host_shared, &receiver, &[host_shared.clone()]), Ok(42));
    for field in ["shared:42", "master:43", "master:42 shared:5", ""] {
        let mut bad = receiver.clone(); bad.optional_fields = field.split_whitespace().map(str::to_owned).collect();
        assert!(mountinfo::publisher(&host_shared, &bad, &[host_shared.clone()]).is_err());
    }
    assert!(mountinfo::publisher(&host_shared, &receiver, &[host_shared.clone(), host_shared.clone()]).is_err());
    let mut stage = evidence("11 1 8:1 /project/agent /stage rw,nosuid,nodev - ext4 /dev/a rw", 1);
    assert!(mountinfo::private_stage(&stage, "/project/agent", "8:1").is_ok());
    for field in ["shared:1", "master:1", "unbindable"] {
        stage.optional_fields = vec![field.into()];
        assert!(mountinfo::private_stage(&stage, "/project/agent", "8:1").is_err());
    }
    assert!(mountinfo::exact_children(&[host.clone(), stage], &host, &[]).is_ok());
    let foreign = evidence("12 10 8:1 /other /publisher/foreign rw - ext4 /dev/a rw", 1);
    assert!(mountinfo::exact_children(&[host.clone(), foreign], &host, &[]).is_err());
}

#[test]
fn mountinfo_malformed_records_fail_closed() {
    for line in ["", "1 2 8:1 / / rw - ext4 x", "1 2 8:1 /bad\\777 / rw - ext4 x rw", "1 2 8:1 / / rw shared:1 shared:2 - ext4 x rw", "1 2 8:1 / / rw alien:1 - ext4 x rw", "1 2 8:1 / / rw,rw - ext4 x rw"] {
        assert!(mountinfo::parse(line.as_bytes(), NamespaceIdentity { device: 1, inode: 1 }).is_err(), "{line}");
    }
}
