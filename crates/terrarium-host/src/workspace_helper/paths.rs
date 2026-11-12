use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2};
use nix::sys::stat::Mode;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use terrarium_core::live_workspace::{Enrollment, Reply};

/// A root whose every ancestor is a real root-owned protected directory.
/// No canonicalize/use window, symlinks, or administrative repairs.
pub struct ProtectedRoot {
    pub(super) fd: File,
    pub(super) ancestors: Vec<File>,
}

fn protected(fd: &File) -> Result<(), Reply> {
    protected_owner(fd, 0)
}

fn protected_owner(fd: &File, owner: u32) -> Result<(), Reply> {
    let m = fd.metadata().map_err(|_| Reply::Failed)?;
    if !m.is_dir() || m.uid() != owner || m.mode() & 0o022 != 0 {
        return Err(Reply::Unauthorized);
    }
    Ok(())
}

fn directory(parent: &File, path: &Path, no_xdev: bool) -> Result<File, Reply> {
    let mut resolve = ResolveFlag::RESOLVE_BENEATH
        | ResolveFlag::RESOLVE_NO_SYMLINKS
        | ResolveFlag::RESOLVE_NO_MAGICLINKS;
    if no_xdev {
        resolve |= ResolveFlag::RESOLVE_NO_XDEV;
    }
    openat2(
        parent,
        path,
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
            .resolve(resolve),
    )
    .map(File::from)
    .map_err(|_| Reply::Unauthorized)
}

impl ProtectedRoot {
    /// An administrative child is created once, never adopted. Registration
    /// IDs are typed, not caller paths. Sync both child and parent before use.
    pub(super) fn create_registration_child(
        &self,
        registration: terrarium_core::live_workspace::RegistrationId,
        owner: u32,
    ) -> Result<Self, Reply> {
        protected_owner(&self.fd, owner)?;
        let name = registration.0.to_string();
        nix::sys::stat::mkdirat(&self.fd, name.as_str(), Mode::S_IRWXU)
            .map_err(|_| Reply::Conflict)?;
        self.fd.sync_all().map_err(|_| Reply::Quarantined)?;
        let child = directory(&self.fd, Path::new(&name), true)?;
        protected_owner(&child, owner)?;
        if mount_id(&child)? != mount_id(&self.fd)? {
            return Err(Reply::Quarantined);
        }
        empty(&child)?;
        child.sync_all().map_err(|_| Reply::Quarantined)?;
        let mut ancestors = self.try_clone()?.ancestors;
        ancestors.push(self.fd.try_clone().map_err(|_| Reply::Failed)?);
        Ok(Self { fd: child, ancestors })
    }

    pub fn open(path: &Path) -> Result<Self, Reply> {
        if !path.is_absolute() {
            return Err(Reply::Unauthorized);
        }
        let mut fd = File::from(
            open(
                "/",
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|_| Reply::Failed)?,
        );
        protected(&fd)?;
        let mut ancestors = Vec::new();
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    ancestors.push(fd.try_clone().map_err(|_| Reply::Failed)?);
                    fd = directory(&fd, Path::new(name), false)?;
                    protected(&fd)?;
                }
                _ => return Err(Reply::Unauthorized),
            }
        }
        Ok(Self { fd, ancestors })
    }

    pub(super) fn try_clone(&self) -> Result<Self, Reply> {
        Ok(Self {
            fd: self.fd.try_clone().map_err(|_| Reply::Failed)?,
            ancestors: self
                .ancestors
                .iter()
                .map(File::try_clone)
                .collect::<std::io::Result<_>>()
                .map_err(|_| Reply::Failed)?,
        })
    }

    #[cfg(test)]
    pub(super) fn fixture(path: &Path) -> Self {
        Self {
            fd: File::open(path).unwrap(),
            ancestors: path
                .ancestors()
                .skip(1)
                .map(|p| File::open(p).unwrap())
                .collect(),
        }
    }

    /// Mapping parents are administrative, the final workspace may be owned by
    /// its holder. B4 is responsible for the exclusive permanent domain seal.
    /// This method pins enrollment identity, not just the current mapping name.
    pub fn enrolled(&self, enrollment: &Enrollment) -> Result<File, Reply> {
        if !enrollment.matches(enrollment) {
            return Err(Reply::Unauthorized);
        }
        let project = directory(&self.fd, Path::new(&enrollment.project), true)?;
        protected(&project)?;
        let source = directory(&project, Path::new(&enrollment.name), true)?;
        let root = self.fd.metadata().map_err(|_| Reply::Failed)?;
        let m = source.metadata().map_err(|_| Reply::Failed)?;
        if m.dev() != root.dev()
            || m.dev() != enrollment.source.device
            || m.ino() != enrollment.source.inode
        {
            return Err(Reply::Unauthorized);
        }
        Ok(source)
    }
}

/// The mount ID comes from the kernel's fdinfo for this already-open FD, not
/// from a pathname lookup. Device equality alone cannot detect same-fs binds.
pub fn mount_id(fd: &File) -> Result<u64, Reply> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let mut text = String::new();
    File::open(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))
        .map_err(|_| Reply::Failed)?
        .take(4097)
        .read_to_string(&mut text)
        .map_err(|_| Reply::Failed)?;
    if text.len() > 4096 {
        return Err(Reply::Failed);
    }
    let values: Vec<_> = text
        .lines()
        .filter_map(|l| l.strip_prefix("mnt_id:"))
        .collect();
    if values.len() != 1 {
        return Err(Reply::Failed);
    }
    values[0].trim().parse().map_err(|_| Reply::Failed)
}

/// Admission-time no-follow walk. Later socket creation cannot be filtered by
/// a bind mount and is constrained by B4's permanent-domain writer isolation.
/// Every object is pinned and checked. Mount crossings, FIFOs, devices, sockets
/// and unknown types fail closed. Symlinks are never followed.
pub fn scan_source(source: &File) -> Result<(), Reply> {
    let expected = mount_id(source)?;
    let mut budget = 100_000usize;
    scan(source, expected, 0, &mut budget)
}

/// Helper-created placeholder pinned beneath protected mapping parents. The
/// source is never mounted onto a pre-existing caller/container directory.
pub struct Destination {
    parent: File,
    name: String,
    slot: File,
    owner: u32,
}

/// Protected parent/leaf identity for ordinary unmount. Holding a File on the
/// mounted root itself would manufacture EBUSY. The parent stays FD-pinned and
/// root-only, while the final mount identity is rechecked immediately before
/// dropping that lookup FD and using a no-follow final-component unmount.
pub(super) struct RevokePoint {
    parent: File,
    name: String,
    source: terrarium_core::live_workspace::SourceIdentity,
    mount: u64,
}
impl RevokePoint {
    fn current(&self) -> Result<File, Reply> {
        protected(&self.parent)?;
        let fd = directory(&self.parent, Path::new(&self.name), false)?;
        let m = fd.metadata().map_err(|_| Reply::Failed)?;
        if (m.dev(), m.ino()) != (self.source.device, self.source.inode)
            || mount_id(&fd)? != self.mount
            || self.mount == mount_id(&self.parent)?
        {
            return Err(Reply::Quarantined);
        }
        Ok(fd)
    }
    pub(super) fn ordinary(&self) -> Result<super::mounts::Unmount, Reply> {
        drop(self.current()?);
        super::mounts::ordinary_unmount_at(&self.parent, &self.name)
    }
    pub(super) fn detach(&self) -> Result<(), Reply> {
        super::mounts::lazy_detach(&self.current()?)
    }
}

fn empty(fd: &File) -> Result<(), Reply> {
    let reopened = directory(fd, Path::new("."), true)?;
    let mut dir = nix::dir::Dir::from_fd(reopened.into()).map_err(|_| Reply::Failed)?;
    for entry in dir.iter() {
        let entry = entry.map_err(|_| Reply::Failed)?;
        if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
            return Err(Reply::Conflict);
        }
    }
    Ok(())
}

impl ProtectedRoot {
    pub(super) fn revoke_point(&self, e: &Enrollment) -> Result<RevokePoint, Reply> {
        if !e.matches(e) {
            return Err(Reply::Unauthorized);
        }
        let parent = directory(&self.fd, Path::new(&e.project), true)?;
        protected(&parent)?;
        let mounted = directory(&parent, Path::new(&e.name), false)?;
        let point = RevokePoint {
            parent,
            name: e.name.clone(),
            source: e.source.clone(),
            mount: mount_id(&mounted)?,
        };
        drop(mounted);
        point.current()?;
        Ok(point)
    }
    /// `self` is the verified per-registration publisher or private staging
    /// root, not the common workspace root. Its namespace topology is checked
    /// independently before this method is used.
    pub fn destination(&self, enrollment: &Enrollment) -> Result<Destination, Reply> {
        self.destination_as(enrollment, 0)
    }

    pub(super) fn workspace_mount(&self, e: &Enrollment) -> Result<File, Reply> {
        if !e.matches(e) {
            return Err(Reply::Unauthorized);
        }
        let parent = directory(&self.fd, Path::new(&e.project), true)?;
        protected(&parent)?;
        directory(&parent, Path::new(&e.name), false)
    }

    /// Recovery after a known, freshly verified attachment was unmounted.
    /// Administrative mapping parents are trusted root-only writers. Unknown
    /// mounts are never adopted or removed, and pending operations quarantine.
    pub(super) fn unmounted_destination(&self, e: &Enrollment) -> Result<Destination, Reply> {
        if !e.matches(e) {
            return Err(Reply::Unauthorized);
        }
        let parent = directory(&self.fd, Path::new(&e.project), true)?;
        protected(&parent)?;
        let slot = directory(&parent, Path::new(&e.name), true)?;
        protected(&slot)?;
        empty(&slot)?;
        let destination = Destination {
            parent,
            name: e.name.clone(),
            slot,
            owner: 0,
        };
        destination.placeholder()?;
        Ok(destination)
    }

    fn destination_as(&self, enrollment: &Enrollment, owner: u32) -> Result<Destination, Reply> {
        use nix::sys::stat::mkdirat;
        if !enrollment.matches(enrollment) {
            return Err(Reply::Unauthorized);
        }
        match mkdirat(
            &self.fd,
            enrollment.project.as_str(),
            Mode::S_IRWXU | Mode::S_IRGRP | Mode::S_IXGRP | Mode::S_IROTH | Mode::S_IXOTH,
        ) {
            Ok(()) => self.fd.sync_all().map_err(|_| Reply::Failed)?,
            Err(nix::errno::Errno::EEXIST) => {}
            Err(_) => return Err(Reply::Failed),
        }
        let parent = directory(&self.fd, Path::new(&enrollment.project), true)?;
        protected_owner(&parent, owner)?;
        mkdirat(
            &parent,
            enrollment.name.as_str(),
            Mode::S_IRWXU | Mode::S_IRGRP | Mode::S_IXGRP | Mode::S_IROTH | Mode::S_IXOTH,
        )
        .map_err(|_| Reply::Conflict)?;
        parent.sync_all().map_err(|_| Reply::Failed)?;
        let slot = directory(&parent, Path::new(&enrollment.name), true)?;
        protected_owner(&slot, owner)?;
        empty(&slot)?;
        Ok(Destination {
            parent,
            name: enrollment.name.clone(),
            slot,
            owner,
        })
    }

    #[cfg(test)]
    pub(super) fn fixture_destination(
        &self,
        enrollment: &Enrollment,
    ) -> Result<Destination, Reply> {
        self.destination_as(enrollment, nix::unistd::geteuid().as_raw())
    }
}

impl Destination {
    pub fn placeholder(&self) -> Result<&File, Reply> {
        let current = directory(&self.parent, Path::new(&self.name), true)?;
        protected_owner(&current, self.owner)?;
        let expected = self.slot.metadata().map_err(|_| Reply::Failed)?;
        let actual = current.metadata().map_err(|_| Reply::Failed)?;
        if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino())
            || mount_id(&current)? != mount_id(&self.parent)?
        {
            return Err(Reply::Conflict);
        }
        empty(&current)?;
        Ok(&self.slot)
    }

    /// After publication, the reopened FD refers to the new mounted tree, not
    /// the underlying placeholder. Kernel evidence must match its mount ID.
    pub fn mounted(&self) -> Result<File, Reply> {
        let current = directory(&self.parent, Path::new(&self.name), false)?;
        if mount_id(&current)? == mount_id(&self.parent)? {
            return Err(Reply::Quarantined);
        }
        Ok(current)
    }

    /// Use only after observing ordinary unmount or lazy detach. Never create
    /// a missing placeholder during revoke recovery or remove an unknown mount.
    pub fn remove(self) -> Result<(), Reply> {
        self.placeholder()?;
        nix::unistd::unlinkat(
            &self.parent,
            self.name.as_str(),
            nix::unistd::UnlinkatFlags::RemoveDir,
        )
        .map_err(|_| Reply::Failed)?;
        self.parent.sync_all().map_err(|_| Reply::Failed)
    }
}

fn scan(parent: &File, expected: u64, depth: usize, budget: &mut usize) -> Result<(), Reply> {
    if depth > 64 || mount_id(parent)? != expected {
        return Err(Reply::Unauthorized);
    }
    let fd = directory(parent, Path::new("."), true)?;
    let mut dir = nix::dir::Dir::from_fd(fd.into()).map_err(|_| Reply::Failed)?;
    for entry in dir.iter() {
        let entry = entry.map_err(|_| Reply::Failed)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        *budget = budget.checked_sub(1).ok_or(Reply::Failed)?;
        let child = File::from(
            openat2(
                parent,
                name,
                OpenHow::new()
                    .flags(OFlag::O_PATH | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
                    .resolve(
                        ResolveFlag::RESOLVE_BENEATH
                            | ResolveFlag::RESOLVE_NO_SYMLINKS
                            | ResolveFlag::RESOLVE_NO_MAGICLINKS
                            | ResolveFlag::RESOLVE_NO_XDEV,
                    ),
            )
            .map_err(|_| Reply::Unauthorized)?,
        );
        let m = child.metadata().map_err(|_| Reply::Failed)?;
        if mount_id(&child)? != expected {
            return Err(Reply::Unauthorized);
        }
        if m.is_dir() {
            scan(&child, expected, depth + 1, budget)?;
        } else if !m.is_file() && !m.file_type().is_symlink() {
            return Err(Reply::Unauthorized);
        }
    }
    Ok(())
}
