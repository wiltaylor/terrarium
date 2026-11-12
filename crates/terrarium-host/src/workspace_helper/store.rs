use super::ProtectedRoot;
use nix::fcntl::{Flock, FlockArg, OFlag, openat, renameat};
use nix::sys::stat::Mode;
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

    fn open_owned(root: ProtectedRoot, owner: u32) -> Result<Self, Reply> {
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
        let file = File::from(
            openat(
                &self.root.fd,
                "journal.json",
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
        let bytes = serde_json::to_vec(journal).map_err(|_| Reply::Failed)?;
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(Reply::Failed);
        }
        let mut next = File::from(
            openat(
                &self.root.fd,
                "journal.next",
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
        renameat(&self.root.fd, "journal.next", &self.root.fd, "journal.json")
            .map_err(|_| Reply::Failed)?;
        self.root.fd.sync_all().map_err(|_| Reply::Failed)
    }
}
