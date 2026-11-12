//! Fixed mount operations on pinned descriptors. No arbitrary flags or paths.
//! These primitives are not reachable from any host verb or production caller.
//! Callers must first persist the helper intent and quarantine every failure.
use rustix::mount::{self, MountPropagationFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags};
use std::fs::File;
use std::os::fd::AsRawFd;
use terrarium_core::live_workspace::Reply;

/// Only mount_setattr is missing a safe wrapper in the pinned dependencies.
/// There is deliberately no permissive fallback implementation.
pub trait Attributes {
    /// Set NOSUID|NODEV, preserving exec and making the mount writable.
    fn workspace(&mut self, mount: &File) -> Result<(), Reply>;
    /// Set NOSUID|NODEV on the administrative publisher. The host remains
    /// writable for placeholder creation/removal. B3's receiver bind is ro.
    fn scaffold(&mut self, mount: &File) -> Result<(), Reply>;
}

pub struct LinuxAttributes;

impl Attributes for LinuxAttributes {
    fn workspace(&mut self, mount: &File) -> Result<(), Reply> {
        use std::os::fd::AsFd;
        use terrarium_sys::{MountAttributes as A, mount_setattr};
        root()?;
        mount_setattr(
            mount.as_fd(),
            A::NO_SUID | A::NO_DEV,
            A::READ_ONLY | A::NO_EXEC,
        )
        .map_err(|_| Reply::Failed)
    }

    fn scaffold(&mut self, mount: &File) -> Result<(), Reply> {
        use std::os::fd::AsFd;
        use terrarium_sys::{MountAttributes as A, mount_setattr};
        root()?;
        mount_setattr(mount.as_fd(), A::NO_SUID | A::NO_DEV, A::READ_ONLY)
            .map_err(|_| Reply::Failed)
    }
}

fn root() -> Result<(), Reply> {
    if !nix::unistd::geteuid().is_root() {
        return Err(Reply::Unauthorized);
    }
    Ok(())
}

fn fd_path(fd: &File) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

/// Clone only this opened directory, never its mounted descendants. The
/// detached FD pins the source until staged/publication completes.
pub fn clone_nonrecursive(source: &File) -> Result<File, Reply> {
    root()?;
    mount::open_tree(
        source,
        "",
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_EMPTY_PATH,
    )
    .map(File::from)
    .map_err(|_| Reply::Failed)
}

/// Both endpoints are directory FDs. Reopen the destination beneath its pinned
/// protected parent after this call, then verify its new mount ID and identity.
pub fn attach(tree: &File, destination: &File) -> Result<(), Reply> {
    root()?;
    mount::move_mount(
        tree,
        "",
        destination,
        "",
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
    )
    .map_err(|_| Reply::Failed)
}

/// Explicitly break inherited propagation BEFORE attribute changes or publish.
/// The caller must verify private mountinfo and persist StageVerified after the
/// attributes are applied. A private flag is not inferred from open_tree.
pub fn make_private(mount: &File) -> Result<(), Reply> {
    root()?;
    mount::mount_change(fd_path(mount), MountPropagationFlags::PRIVATE).map_err(|_| Reply::Failed)
}

/// Only an independently isolated, empty publisher may become shared. The
/// caller verifies a unique host shared group and its receiver relationship.
pub fn make_shared(mount: &File) -> Result<(), Reply> {
    root()?;
    mount::mount_change(fd_path(mount), MountPropagationFlags::SHARED).map_err(|_| Reply::Failed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unmount {
    Removed,
    Busy,
}

/// Ordinary unmount first. EBUSY is the only condition permitting a separately
/// recorded lazy-detach step. Every other error leaves revoke pending.
pub fn ordinary_unmount(mount: &File) -> Result<Unmount, Reply> {
    root()?;
    // umount follows this helper-generated proc FD magic link intentionally.
    // NOFOLLOW would refuse the pinned FD itself. No request path is resolved.
    match mount::unmount(fd_path(mount), UnmountFlags::empty()) {
        Ok(()) => Ok(Unmount::Removed),
        Err(rustix::io::Errno::BUSY) => Ok(Unmount::Busy),
        Err(_) => Err(Reply::Failed),
    }
}

pub fn lazy_detach(mount: &File) -> Result<(), Reply> {
    root()?;
    mount::unmount(fd_path(mount), UnmountFlags::DETACH).map_err(|_| Reply::Failed)
}
