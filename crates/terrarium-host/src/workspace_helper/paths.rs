use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2};
use nix::sys::stat::Mode;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use terrarium_core::live_workspace::{Enrollment, Reply};

/// A root whose every ancestor is a real root-owned protected directory.
/// No canonicalize/use window, symlinks, or administrative repairs.
pub struct ProtectedRoot { pub(super) fd: File }

fn protected(fd: &File) -> Result<(), Reply> {
    let m = fd.metadata().map_err(|_| Reply::Failed)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 { return Err(Reply::Unauthorized); }
    Ok(())
}

fn directory(parent: &File, path: &Path, no_xdev: bool) -> Result<File, Reply> {
    let mut resolve = ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS;
    if no_xdev { resolve |= ResolveFlag::RESOLVE_NO_XDEV; }
    openat2(parent, path, OpenHow::new().flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW).resolve(resolve))
        .map(File::from).map_err(|_| Reply::Unauthorized)
}

impl ProtectedRoot {
    pub fn open(path: &Path) -> Result<Self, Reply> {
        if !path.is_absolute() { return Err(Reply::Unauthorized); }
        let mut fd = File::from(open("/", OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW, Mode::empty()).map_err(|_| Reply::Failed)?);
        protected(&fd)?;
        for part in path.components() {
            match part {
                Component::RootDir => {},
                Component::Normal(name) => { fd = directory(&fd, Path::new(name), false)?; protected(&fd)?; },
                _ => return Err(Reply::Unauthorized),
            }
        }
        Ok(Self { fd })
    }

    /// Mapping parents are administrative, the final workspace may be owned by
    /// its holder. B4 is responsible for the exclusive permanent domain seal.
    /// This method pins enrollment identity, not just the current mapping name.
    pub fn enrolled(&self, enrollment: &Enrollment) -> Result<File, Reply> {
        if !enrollment.matches(enrollment) { return Err(Reply::Unauthorized); }
        let project = directory(&self.fd, Path::new(&enrollment.project), true)?;
        protected(&project)?;
        let source = directory(&project, Path::new(&enrollment.name), true)?;
        let root = self.fd.metadata().map_err(|_| Reply::Failed)?;
        let m = source.metadata().map_err(|_| Reply::Failed)?;
        if m.dev() != root.dev() || m.dev() != enrollment.source.device || m.ino() != enrollment.source.inode {
            return Err(Reply::Unauthorized);
        }
        Ok(source)
    }
}
