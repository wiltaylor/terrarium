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
}

fn protected(fd: &File) -> Result<(), Reply> {
    let m = fd.metadata().map_err(|_| Reply::Failed)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
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
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    fd = directory(&fd, Path::new(name), false)?;
                    protected(&fd)?;
                }
                _ => return Err(Reply::Unauthorized),
            }
        }
        Ok(Self { fd })
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
        .map_err(|_| Reply::Failed)?.take(4097).read_to_string(&mut text).map_err(|_| Reply::Failed)?;
    if text.len() > 4096 { return Err(Reply::Failed); }
    let values: Vec<_> = text.lines().filter_map(|l| l.strip_prefix("mnt_id:")).collect();
    if values.len() != 1 { return Err(Reply::Failed); }
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

fn scan(parent: &File, expected: u64, depth: usize, budget: &mut usize) -> Result<(), Reply> {
    if depth > 64 || mount_id(parent)? != expected { return Err(Reply::Unauthorized); }
    let fd = directory(parent, Path::new("."), true)?;
    let mut dir = nix::dir::Dir::from_fd(fd.into()).map_err(|_| Reply::Failed)?;
    for entry in dir.iter() {
        let entry = entry.map_err(|_| Reply::Failed)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") { continue; }
        *budget = budget.checked_sub(1).ok_or(Reply::Failed)?;
        let child = File::from(openat2(parent, name, OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS | ResolveFlag::RESOLVE_NO_XDEV))
            .map_err(|_| Reply::Unauthorized)?);
        let m = child.metadata().map_err(|_| Reply::Failed)?;
        if mount_id(&child)? != expected { return Err(Reply::Unauthorized); }
        if m.is_dir() {
            scan(&child, expected, depth + 1, budget)?;
        } else if !m.is_file() && !m.file_type().is_symlink() {
            return Err(Reply::Unauthorized);
        }
    }
    Ok(())
}
