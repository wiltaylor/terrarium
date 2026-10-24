//! Opens of the private files with no symlink follow (`docs/security-review.md`,
//! F11). Before the split of the agent dir, the harness could write every
//! file in it. A symlink that it planted there must not send the records of
//! the runner (with the injected keys) to a place that the harness can read.
//!
//! Every open here adds `O_NOFOLLOW`, so a symlink in the last component
//! fails with `ELOOP`. [`check_private_dir`] refuses a symlink at the agent
//! dir and at `private/`. The opens also refuse a file that is not a
//! regular file (a FIFO, a device, a socket).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use nix::fcntl::OFlag;

use crate::error::{Result, RunnerError};

/// The private files that the runner and the daemon keep in `private/`. An
/// entry with one of these names must be a regular file.
pub const PRIVATE_FILES: &[&str] = &[
    "events.jsonl",
    "pty.log",
    "pty.log.1",
    "runner.json",
    "runner-argv.json",
    "runner-exit.json",
    "runner-exit.json.tmp",
    "runner.log",
    "session-prompt.sha256",
];

fn nofollow() -> i32 {
    OFlag::O_NOFOLLOW.bits()
}

/// An error for a path that is a symlink or the wrong file type.
fn unsafe_path(path: &Path, reason: &'static str) -> RunnerError {
    RunnerError::UnsafePath {
        path: path.to_path_buf(),
        reason,
    }
}

/// Map an open error: `ELOOP` from `O_NOFOLLOW` is a symlink.
fn open_err(action: &'static str, path: &Path, e: std::io::Error) -> RunnerError {
    if e.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) {
        unsafe_path(path, "is a symlink")
    } else {
        RunnerError::io(action, path, e)
    }
}

/// Open `path` with `opts` and `O_NOFOLLOW`, and refuse a file that is not
/// regular.
pub(crate) fn open(path: &Path, opts: &mut OpenOptions) -> Result<File> {
    // `O_NONBLOCK` keeps an open of a FIFO from blocking; the type check
    // below then refuses it. It has no effect on a regular file.
    let file = opts
        .custom_flags(nofollow() | OFlag::O_NONBLOCK.bits())
        .open(path)
        .map_err(|e| open_err("open", path, e))?;
    let meta = file
        .metadata()
        .map_err(|e| RunnerError::io("stat", path, e))?;
    if !meta.file_type().is_file() {
        return Err(unsafe_path(path, "is not a regular file"));
    }
    // Clear `O_NONBLOCK` again, and keep the other flags (`O_APPEND`).
    if let Ok(bits) = nix::fcntl::fcntl(&file, nix::fcntl::FcntlArg::F_GETFL) {
        let flags = OFlag::from_bits_truncate(bits) - OFlag::O_NONBLOCK;
        let _ = nix::fcntl::fcntl(&file, nix::fcntl::FcntlArg::F_SETFL(flags));
    }
    Ok(file)
}

/// Open `path` for read, with no symlink follow.
pub(crate) fn open_read(path: &Path) -> Result<File> {
    open(path, OpenOptions::new().read(true))
}

/// Read `path` as text, with no symlink follow.
pub(crate) fn read_to_string(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut s = String::new();
    open_read(path)?
        .read_to_string(&mut s)
        .map_err(|e| RunnerError::io("read", path, e))?;
    Ok(s)
}

/// Open `path` for append (made with mode `0600` when it is new), with no
/// symlink follow.
pub(crate) fn open_append(path: &Path) -> Result<File> {
    open(
        path,
        OpenOptions::new().create(true).append(true).mode(0o600),
    )
}

/// Write `bytes` to `path` with mode `0600`, with no symlink follow. An old
/// file is truncated, and its mode becomes `0600`.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = open(
        path,
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600),
    )?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| RunnerError::io("chmod", path, e))?;
    f.write_all(bytes)
        .map_err(|e| RunnerError::io("write", path, e))
}

/// Write `bytes` to `path` through `<path>.tmp` and a rename, so that a
/// reader never sees half a file. The temp file is made new (`O_EXCL`):
/// an old temp file, or a symlink in its place, is removed first. `rename`
/// replaces a symlink at `path` itself, never its target.
pub(crate) fn write_atomic(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<()> {
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(RunnerError::io("remove", tmp, e)),
    }
    let mut f = open(
        tmp,
        OpenOptions::new().write(true).create_new(true).mode(0o600),
    )?;
    f.write_all(bytes)
        .map_err(|e| RunnerError::io("write", tmp, e))?;
    drop(f);
    std::fs::rename(tmp, path).map_err(|e| RunnerError::io("rename", tmp, e))
}

/// Check that `path` is a real directory (not a symlink), when it exists.
/// True: it exists.
fn check_dir(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(unsafe_path(path, "is a symlink")),
        Ok(m) if !m.file_type().is_dir() => Err(unsafe_path(path, "is not a directory")),
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(RunnerError::io("stat", path, e)),
    }
}

/// Make the private directory `dir` (and its parent, the agent directory),
/// and set mode `0700`. Refuse ([`RunnerError::UnsafePath`]) when the agent
/// dir or `dir` is a symlink or not a directory, or when an entry in `dir`
/// with a name of [`PRIVATE_FILES`] is a symlink or not a regular file.
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    if let Some(agent) = dir.parent().filter(|p| !p.as_os_str().is_empty())
        && !check_dir(agent)?
    {
        std::fs::create_dir_all(agent).map_err(|e| RunnerError::io("create", agent, e))?;
        check_dir(agent)?;
    }
    if !check_dir(dir)? {
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            // Another runner made it at the same time; the checks below
            // look at it.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(RunnerError::io("create", dir, e)),
        }
    }
    // Set the mode through a handle on the directory itself: a symlink that
    // took its place since the check fails the open.
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow() | OFlag::O_DIRECTORY.bits())
        .open(dir)
        .map_err(|e| match e.raw_os_error() {
            Some(n) if n == nix::errno::Errno::ELOOP as i32 => unsafe_path(dir, "is a symlink"),
            Some(n) if n == nix::errno::Errno::ENOTDIR as i32 => {
                unsafe_path(dir, "is not a directory")
            }
            _ => RunnerError::io("open", dir, e),
        })?;
    handle
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .map_err(|e| RunnerError::io("chmod", dir, e))?;
    for name in PRIVATE_FILES {
        let path = dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(unsafe_path(&path, "is a symlink"));
            }
            Ok(m) if !m.file_type().is_file() => {
                return Err(unsafe_path(&path, "is not a regular file"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(RunnerError::io("stat", &path, e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn is_unsafe(r: Result<impl std::fmt::Debug>, want: &Path) -> String {
        match r {
            Err(e @ RunnerError::UnsafePath { .. }) => {
                let text = e.to_string();
                assert!(text.contains(&want.display().to_string()), "{text}");
                text
            }
            other => panic!("want UnsafePath for {}, got {other:?}", want.display()),
        }
    }

    #[test]
    fn a_new_private_dir_is_0700() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("agents/a/private");
        ensure_private_dir(&dir).expect("create");
        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // An existing dir with a wide mode gets 0700 again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::write(dir.join("events.jsonl"), b"").expect("write");
        ensure_private_dir(&dir).expect("again");
        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_symlink_at_private_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let agent = tmp.path().join("a");
        let work = agent.join("workspace");
        std::fs::create_dir_all(&work).expect("mk");
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let dir = agent.join("private");
        symlink(&work, &dir).expect("plant");
        let text = is_unsafe(ensure_private_dir(&dir), &dir);
        assert!(text.contains("symlink"), "{text}");
        // The target keeps its mode: nothing followed the link.
        let mode = std::fs::metadata(&work).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn a_symlink_at_the_agent_dir_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("mk");
        let agent = tmp.path().join("a");
        symlink(&elsewhere, &agent).expect("plant");
        is_unsafe(ensure_private_dir(&agent.join("private")), &agent);
        assert!(!elsewhere.join("private").exists());
    }

    #[test]
    fn a_file_at_private_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("a/private");
        std::fs::create_dir_all(tmp.path().join("a")).expect("mk");
        std::fs::write(&dir, b"x").expect("write");
        is_unsafe(ensure_private_dir(&dir), &dir);
    }

    #[test]
    fn a_planted_record_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("a/private");
        ensure_private_dir(&dir).expect("create");
        let target = tmp.path().join("target");
        std::fs::write(&target, b"").expect("write");
        let runner_json = dir.join("runner.json");
        symlink(&target, &runner_json).expect("plant");
        is_unsafe(ensure_private_dir(&dir), &runner_json);
        // The opens refuse it too, and the target stays empty.
        is_unsafe(write_private(&runner_json, b"secret"), &runner_json);
        is_unsafe(open_append(&runner_json), &runner_json);
        is_unsafe(read_to_string(&runner_json), &runner_json);
        assert_eq!(std::fs::read(&target).expect("read"), b"");
        // A directory with a record name is refused.
        std::fs::remove_file(&runner_json).expect("rm");
        std::fs::create_dir(dir.join("runner.log")).expect("mk");
        is_unsafe(ensure_private_dir(&dir), &dir.join("runner.log"));
    }

    #[test]
    fn a_fifo_is_refused_without_a_block() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("events.jsonl");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
        is_unsafe(open_read(&path), &path);
    }

    #[test]
    fn write_atomic_replaces_a_symlink_and_not_its_target() {
        let tmp = tempfile::tempdir().expect("tmp");
        let target = tmp.path().join("target");
        std::fs::write(&target, b"keep").expect("write");
        let path = tmp.path().join("runner-exit.json");
        let tmp_path = tmp.path().join("runner-exit.json.tmp");
        symlink(&target, &path).expect("plant");
        symlink(&target, &tmp_path).expect("plant tmp");
        write_atomic(&path, &tmp_path, b"{}").expect("write");
        assert_eq!(std::fs::read(&target).expect("read"), b"keep");
        assert!(
            !std::fs::symlink_metadata(&path)
                .expect("stat")
                .file_type()
                .is_symlink()
        );
        assert_eq!(read_to_string(&path).expect("read"), "{}");
        assert!(!tmp_path.exists());
    }
}
