//! Local observations only. Kernel strings and a boot ID are not runtime proof,
//! approval, expected policy, or evidence of Engine/probe identity.
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::Mode;
use std::fs::File;
use std::io::Read;
use terrarium_core::live_workspace::Reply;

const LIMIT: usize = 512;

#[derive(Debug, PartialEq, Eq)]
pub struct HostBaseline {
    pub kernel_release: String,
    pub kernel_build: String,
    pub architecture: String,
    pub boot_id: String,
}

fn text(bytes: &[u8]) -> Result<String, Reply> {
    if bytes.is_empty() || bytes.len() > LIMIT {
        return Err(Reply::Unsupported);
    }
    let value = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if value.is_empty() || !value.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return Err(Reply::Unsupported);
    }
    String::from_utf8(value.to_vec()).map_err(|_| Reply::Unsupported)
}

fn boot_id(bytes: &[u8]) -> Result<String, Reply> {
    let value = text(bytes)?;
    if value.len() != 36
        || !value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
        || value.bytes().all(|b| b == b'0' || b == b'-')
    {
        return Err(Reply::Unsupported);
    }
    Ok(value)
}

fn child(parent: &File, name: &str, directory: bool) -> Result<File, Reply> {
    let mut flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK;
    if directory {
        flags |= OFlag::O_DIRECTORY;
    }
    let file =
        File::from(openat(parent, name, flags, Mode::empty()).map_err(|_| Reply::Unsupported)?);
    if nix::sys::statfs::fstatfs(&file)
        .map_err(|_| Reply::Unsupported)?
        .filesystem_type()
        != nix::sys::statfs::PROC_SUPER_MAGIC
    {
        return Err(Reply::Unsupported);
    }
    Ok(file)
}

fn read(parent: &File, name: &str) -> Result<Vec<u8>, Reply> {
    let mut bytes = Vec::new();
    child(parent, name, false)?
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Reply::Unsupported)?;
    Ok(bytes)
}

/// Observe fixed local procfs entries, never caller paths or Engine PIDs.
/// No subprocess, Engine request, mount, namespace entry or probe is invoked.
pub fn collect_local() -> Result<HostBaseline, Reply> {
    let proc = File::from(
        open(
            "/proc",
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|_| Reply::Unsupported)?,
    );
    let sys = child(&proc, "sys", true)?;
    let kernel = child(&sys, "kernel", true)?;
    let random = child(&kernel, "random", true)?;
    let before = boot_id(&read(&random, "boot_id")?)?;
    // SAFETY: uname writes only to the provided, correctly sized utsname.
    let mut uts = std::mem::MaybeUninit::<nix::libc::utsname>::uninit();
    if unsafe { nix::libc::uname(uts.as_mut_ptr()) } != 0 {
        return Err(Reply::Unsupported);
    }
    // SAFETY: successful uname initialized all fields, including terminating NULs.
    let uts = unsafe { uts.assume_init() };
    let field = |chars: &[nix::libc::c_char]| {
        let bytes: Vec<u8> = chars
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        text(&bytes)
    };
    let observed = HostBaseline {
        kernel_release: field(&uts.release)?,
        kernel_build: field(&uts.version)?,
        architecture: field(&uts.machine)?,
        boot_id: before,
    };
    if observed.kernel_release != text(&read(&kernel, "osrelease")?)?
        || observed.kernel_build != text(&read(&kernel, "version")?)?
        || observed.boot_id != boot_id(&read(&random, "boot_id")?)?
    {
        return Err(Reply::Unsupported);
    }
    Ok(observed)
}

/// Terminal gate: no reviewed Engine/process binding, trusted exact-context
/// probe acquisition or functional approval exists. Even a complete baseline
/// cannot authorize effects. There is deliberately no success constructor.
pub fn current_admission() -> Result<(), Reply> {
    Err(Reply::Unsupported)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn bounded_kernel_parser_rejects_truncation_controls_and_empty_values() {
        assert_eq!(text(b"6.12.1-test\n").unwrap(), "6.12.1-test");
        assert_eq!(
            text(b"#1 SMP PREEMPT_DYNAMIC").unwrap(),
            "#1 SMP PREEMPT_DYNAMIC"
        );
        for bytes in [
            b"".as_slice(),
            b"\n",
            b"x\ny",
            b"x\0",
            b"x\r\n",
            &[255],
            &vec![b'x'; LIMIT + 1],
        ] {
            assert_eq!(text(bytes), Err(Reply::Unsupported));
        }
    }

    #[test]
    fn boot_parser_requires_one_complete_canonical_nonzero_id() {
        let id = "12345678-1234-1234-abcd-123456789abc";
        assert_eq!(boot_id(format!("{id}\n").as_bytes()).unwrap(), id);
        for value in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "12345678-1234-1234-ABCD-123456789ABC",
            "12345678-1234-1234-abcd-123456789ab",
            "12345678_1234-1234-abcd-123456789abc",
        ] {
            assert_eq!(boot_id(value.as_bytes()), Err(Reply::Unsupported));
        }
    }

    #[test]
    fn missing_engine_probe_and_approval_are_terminal_without_effects() {
        assert_eq!(current_admission(), Err(Reply::Unsupported));
    }
}
