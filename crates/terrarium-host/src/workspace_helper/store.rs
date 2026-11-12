use super::ProtectedRoot;
use nix::fcntl::{Flock, FlockArg, OFlag, openat, renameat};
use nix::sys::stat::Mode;
use serde::{Serialize, de::DeserializeOwned};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use terrarium_core::live_workspace::{Journal, Reply};

const MAX_JOURNAL: u64 = 8 * 1024 * 1024;

/// Directory-FD relative snapshot store with a lifetime exclusive lock.
/// A leftover `.next` snapshot means a prior write was not completed and is
/// refused, never silently discarded or treated as a committed result.
pub struct Store {
    root: ProtectedRoot,
    _lock: Flock<File>,
    owner: u32,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod bootstrap_tests {
    use super::*;
    use terrarium_core::live_workspace::bootstrap::{
        Journal as Bootstrap, Request, Scaffold, Step,
    };
    use terrarium_core::live_workspace::{Caller, PROTOCOL_VERSION, Provenance, SourceIdentity};

    fn id<T: serde::de::DeserializeOwned>(n: u64) -> T {
        serde_json::from_str(&format!("\"{n:026}\"")).unwrap()
    }

    fn initial() -> Bootstrap {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let n = NEXT.fetch_add(10, Ordering::Relaxed);
        Bootstrap::new(Request {
            version: PROTOCOL_VERSION,
            operation: id(n),
            provenance: Provenance {
                daemon_instance: id(n + 1),
                caller: Caller::LocalOperator,
            },
            host: id(n + 2),
            target: "synthetic".into(),
            registration: id(n + 3),
            incarnation: id(n + 4),
            domain: id(n + 5),
        })
        .unwrap()
    }

    fn scaffold() -> Step {
        Step::ScaffoldVerified(Scaffold {
            publisher: SourceIdentity {
                device: 1,
                inode: 1,
            },
            staging: SourceIdentity {
                device: 1,
                inode: 2,
            },
            state: SourceIdentity {
                device: 1,
                inode: 3,
            },
        })
    }

    #[test]
    fn bootstrap_snapshot_is_locked_ordered_and_identity_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::fixture(dir.path()).unwrap();
        assert!(matches!(Store::fixture(dir.path()), Err(Reply::Busy)));
        let first = initial();
        store.begin_bootstrap(&first).unwrap();
        assert_eq!(store.begin_bootstrap(&first), Err(Reply::Conflict));
        assert_eq!(store.load_bootstrap().unwrap(), first);
        let mut next = first.clone();
        next.advance(scaffold()).unwrap();
        store.save_bootstrap(&next).unwrap();
        assert_eq!(store.save_bootstrap(&first), Err(Reply::Conflict));
        assert_eq!(store.save_bootstrap(&initial()), Err(Reply::Conflict));
        next.advance(Step::CreatePending).unwrap();
        next.advance(Step::ContainerBound("a".repeat(64))).unwrap();
        // Neither observation may bypass its own durable boundary.
        assert_eq!(store.save_bootstrap(&next), Err(Reply::Conflict));
        assert_eq!(store.load_bootstrap().unwrap().original(), first.original());
    }

    #[test]
    fn bootstrap_cannot_start_after_a_side_effect_or_clear_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::fixture(dir.path()).unwrap();
        let first = initial();
        let mut advanced = first.clone();
        advanced.advance(scaffold()).unwrap();
        assert_eq!(store.save_bootstrap(&advanced), Err(Reply::Quarantined));
        store.save_bootstrap(&first).unwrap();
        let mut fenced = first.clone();
        fenced.recover_without_evidence();
        store.save_bootstrap(&fenced).unwrap();
        assert_eq!(store.save_bootstrap(&first), Err(Reply::Conflict));
        assert_eq!(store.save_bootstrap(&advanced), Err(Reply::Conflict));
    }

    #[test]
    fn bootstrap_foreign_state_and_partial_write_are_never_adopted() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::fixture(dir.path()).unwrap();
        let canary = dir.path().join("canary");
        std::fs::write(&canary, b"unchanged").unwrap();
        symlink(&canary, dir.path().join("bootstrap.json")).unwrap();
        assert!(store.save_bootstrap(&initial()).is_err());
        assert_eq!(std::fs::read(&canary).unwrap(), b"unchanged");
        std::fs::remove_file(dir.path().join("bootstrap.json")).unwrap();
        std::fs::write(dir.path().join("bootstrap.json"), b"unknown history").unwrap();
        assert!(store.save_bootstrap(&initial()).is_err());
        std::fs::write(dir.path().join("bootstrap.next"), b"interrupted").unwrap();
        drop(store);
        assert!(matches!(
            Store::fixture(dir.path()),
            Err(Reply::Quarantined)
        ));
    }
}

fn checked(file: &File, owner: u32) -> Result<(), Reply> {
    let m = file.metadata().map_err(|_| Reply::Failed)?;
    if !m.is_file() || m.uid() != owner || m.mode() & 0o077 != 0 || m.nlink() != 1 {
        return Err(Reply::Unauthorized);
    }
    Ok(())
}

impl Store {
    pub fn open(root: ProtectedRoot) -> Result<Self, Reply> {
        Self::open_owned(root, 0)
    }

    pub(super) fn open_owned(root: ProtectedRoot, owner: u32) -> Result<Self, Reply> {
        let file = File::from(
            openat(
                &root.fd,
                "lock",
                OFlag::O_RDWR
                    | OFlag::O_CREAT
                    | OFlag::O_CLOEXEC
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_NONBLOCK,
                Mode::S_IRUSR | Mode::S_IWUSR,
            )
            .map_err(|_| Reply::Failed)?,
        );
        checked(&file, owner)?;
        let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|_| Reply::Busy)?;
        // Inspect without following symlinks or opening FIFOs/devices.
        match nix::sys::stat::fstatat(
            &root.fd,
            "journal.next",
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(nix::errno::Errno::ENOENT) => {}
            _ => return Err(Reply::Quarantined),
        }
        match nix::sys::stat::fstatat(
            &root.fd,
            "bootstrap.next",
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(nix::errno::Errno::ENOENT) => {}
            _ => return Err(Reply::Quarantined),
        }
        Ok(Self {
            root,
            _lock: lock,
            owner,
        })
    }

    #[cfg(test)]
    pub(super) fn fixture(path: &std::path::Path) -> Result<Self, Reply> {
        Self::open_owned(
            ProtectedRoot {
                fd: File::open(path).map_err(|_| Reply::Failed)?,
                ancestors: Vec::new(),
            },
            nix::unistd::geteuid().as_raw(),
        )
    }

    /// An initial journal is provisioned only by the isolated fixture/bootstrap
    /// caller. Missing or unreadable journals are never implicitly recreated.
    pub fn load(&self) -> Result<Journal, Reply> {
        self.load_named("journal.json")
    }

    /// Bootstrap has its own immutable request because the runtime does not
    /// exist before Docker create. It shares the registration lifetime lock.
    pub fn load_bootstrap(
        &self,
    ) -> Result<terrarium_core::live_workspace::bootstrap::Journal, Reply> {
        self.load_named("bootstrap.json")
    }

    fn load_named<T: DeserializeOwned>(&self, name: &str) -> Result<T, Reply> {
        let file = File::from(
            openat(
                &self.root.fd,
                name,
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
                Mode::empty(),
            )
            .map_err(|_| Reply::Failed)?,
        );
        checked(&file, self.owner)?;
        let mut bytes = Vec::new();
        file.take(MAX_JOURNAL + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Reply::Failed)?;
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(Reply::Failed);
        }
        serde_json::from_slice(&bytes).map_err(|_| Reply::Failed)
    }

    pub fn save(&self, journal: &Journal) -> Result<(), Reply> {
        self.save_named(journal, "journal.next", "journal.json")
    }

    /// Install ownership once under the lifetime lock. Even an identical
    /// existing intent requires recovery, not another create attempt.
    pub fn begin_bootstrap(
        &self,
        journal: &terrarium_core::live_workspace::bootstrap::Journal,
    ) -> Result<(), Reply> {
        match nix::sys::stat::fstatat(
            &self.root.fd,
            "bootstrap.json",
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(nix::errno::Errno::ENOENT) => self.save_bootstrap(journal),
            Ok(_) => Err(Reply::Conflict),
            Err(_) => Err(Reply::Quarantined),
        }
    }

    pub fn save_bootstrap(
        &self,
        journal: &terrarium_core::live_workspace::bootstrap::Journal,
    ) -> Result<(), Reply> {
        // A caller must not replace the initial intent with another identity.
        // Missing state is permitted only for the initial durable write. A
        // malformed, symlinked or otherwise unreadable existing file refuses.
        match nix::sys::stat::fstatat(
            &self.root.fd,
            "bootstrap.json",
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(nix::errno::Errno::ENOENT) if journal.initial() => {}
            Err(nix::errno::Errno::ENOENT) => return Err(Reply::Quarantined),
            Ok(_) => {
                let old = self.load_bootstrap()?;
                if !journal.follows(&old) {
                    return Err(Reply::Conflict);
                }
            }
            Err(_) => return Err(Reply::Quarantined),
        }
        self.save_named(journal, "bootstrap.next", "bootstrap.json")
    }

    fn save_named<T: Serialize>(
        &self,
        journal: &T,
        next_name: &str,
        final_name: &str,
    ) -> Result<(), Reply> {
        let bytes = serde_json::to_vec(journal).map_err(|_| Reply::Failed)?;
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(Reply::Failed);
        }
        let mut next = File::from(
            openat(
                &self.root.fd,
                next_name,
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_CLOEXEC
                    | OFlag::O_NOFOLLOW,
                Mode::S_IRUSR | Mode::S_IWUSR,
            )
            .map_err(|_| Reply::Failed)?,
        );
        checked(&next, self.owner)?;
        next.write_all(&bytes).map_err(|_| Reply::Failed)?;
        next.sync_all().map_err(|_| Reply::Failed)?;
        // Persist the created name as well as the file, before replacement.
        self.root.fd.sync_all().map_err(|_| Reply::Failed)?;
        renameat(&self.root.fd, next_name, &self.root.fd, final_name).map_err(|_| Reply::Failed)?;
        self.root.fd.sync_all().map_err(|_| Reply::Failed)
    }
}
