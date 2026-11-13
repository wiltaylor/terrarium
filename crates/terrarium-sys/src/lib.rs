//! Audited Linux syscall wrappers only. No policy, transport or mount paths.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "linux")]
use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd},
};

bitflags::bitflags! {
    /// The supported non-recursive mount_setattr attributes. No ID mapping,
    /// user-namespace FD, propagation flags, or arbitrary kernel bits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MountAttributes: u64 {
        const READ_ONLY = 0x0000_0001;
        const NO_SUID = 0x0000_0002;
        const NO_DEV = 0x0000_0004;
        const NO_EXEC = 0x0000_0008;
    }
}

#[repr(C)]
#[derive(Debug, Default)]
#[cfg(any(target_os = "linux", test))]
struct MountAttr {
    set: u64,
    clear: u64,
    propagation: u64,
    userns_fd: u64,
}

#[cfg(any(target_os = "linux", test))]
fn attributes(set: MountAttributes, clear: MountAttributes) -> std::io::Result<MountAttr> {
    if set.intersects(clear) || (set.bits() | clear.bits()) & !MountAttributes::all().bits() != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid mount attributes",
        ));
    }
    Ok(MountAttr {
        set: set.bits(),
        clear: clear.bits(),
        propagation: 0,
        userns_fd: 0,
    })
}

/// Apply attributes to exactly the pinned mount, non-recursively. The kernel
/// checks privilege and mount-FD validity. Borrowing cannot close the FD while
/// the syscall runs. No raw pointer or request-selected path crosses the API.
#[cfg(target_os = "linux")]
pub fn mount_setattr(
    fd: BorrowedFd<'_>,
    set: MountAttributes,
    clear: MountAttributes,
) -> io::Result<()> {
    let attr = attributes(set, clear)?;
    // SAFETY: attr is a fully initialized repr(C) Linux mount_attr v0 (four
    // u64s), correctly aligned and alive until the synchronous syscall returns.
    // The static empty C string is NUL terminated. AT_EMPTY_PATH addresses only
    // the borrowed live FD. Size matches the supplied allocation, no recursive
    // or ID-map flags are passed, and the kernel cannot retain these pointers.
    let result = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attribute_layout_and_fixed_bits() {
        assert_eq!(std::mem::size_of::<MountAttr>(), 32);
        assert_eq!(
            std::mem::align_of::<MountAttr>(),
            std::mem::align_of::<u64>()
        );
        let a = attributes(
            MountAttributes::NO_SUID | MountAttributes::NO_DEV,
            MountAttributes::READ_ONLY | MountAttributes::NO_EXEC,
        )
        .unwrap();
        assert_eq!((a.set, a.clear, a.propagation, a.userns_fd), (6, 9, 0, 0));
    }
    #[test]
    fn overlapping_and_unknown_bits_are_refused_before_syscall() {
        assert!(attributes(MountAttributes::NO_DEV, MountAttributes::NO_DEV).is_err());
        assert!(
            attributes(
                MountAttributes::from_bits_retain(1 << 63),
                MountAttributes::empty()
            )
            .is_err()
        );
        assert!(
            attributes(
                MountAttributes::empty(),
                MountAttributes::from_bits_retain(1 << 63)
            )
            .is_err()
        );
    }
}
