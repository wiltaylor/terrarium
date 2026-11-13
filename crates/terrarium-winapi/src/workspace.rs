//! Account workspaces under `C:\Terrarium\work\<project>\<agent>` (TER-274).
//!
//! Every component from the drive root to the leaf is opened by itself
//! (`FILE_FLAG_OPEN_REPARSE_POINT`), refused when it is a reparse point or
//! the wrong type, and refused when its normalized final path differs from
//! the requested spelling (8.3 short names, other aliases). Each checked
//! directory stays open without `FILE_SHARE_DELETE` for the whole operation,
//! so it cannot be renamed or replaced while later components are opened.
//! A push publishes and a failed push deletes its temporary file through the
//! file handle, never through a second path lookup.
//!
//! Isolation is Windows account authority only: a different ordinary account
//! has no access to the workspace DACL. The same account and administrators
//! are not isolated, and a command that runs as the account keeps its rights.

use super::*;
use crate::policy_file::{Local, audit_acl, sid_from_string, trusted_sids};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Storage::FileSystem::*;

/// The workspace base. It must exist: the host never creates it.
pub const BASE: &str = r"C:\Terrarium\work";
const ANCESTORS: [&str; 2] = [r"C:\", r"C:\Terrarium"];

/// Ancestors may let users create unrelated children, but not delete,
/// replace or re-permission the directory (the `policy_file` rule).
const ANCESTOR_FORBIDDEN: u32 =
    DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL | GENERIC_WRITE | FILE_DELETE_CHILD;
/// The base and project directories also refuse untrusted child creation, so
/// no user can pre-create a project or agent directory that it owns.
const STRICT_FORBIDDEN: u32 = ANCESTOR_FORBIDDEN
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES;
const DIR_ACCESS: u32 = READ_CONTROL | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const INNER_ACCESS: u32 = FILE_READ_ATTRIBUTES | SYNCHRONIZE;

fn refused(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("workspace path refused: {what}"),
    )
}

/// One path component: ASCII, no separator, stream, wildcard or control
/// character, no `.`/`..`, no trailing dot or space, no DOS device name.
pub fn check_name(name: &str) -> io::Result<()> {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_lowercase();
    let device = matches!(
        stem.as_str(),
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) || (stem.len() == 4
        && (stem.starts_with("com") || stem.starts_with("lpt"))
        && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if name.is_empty()
        || !name.is_ascii()
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_ascii_control() || "<>:\"/\\|?*".contains(c))
        || device
    {
        return Err(refused("invalid component name"));
    }
    Ok(())
}

fn info(handle: &impl AsRawHandle) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: valid all-zero POD, then filled by the call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    // SAFETY: live handle and writable POD output.
    checked(unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) })?;
    Ok(info)
}

fn final_path(handle: &impl AsRawHandle) -> io::Result<String> {
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: live handle and a writable buffer of the stated length.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buffer.len() {
            buffer.truncate(n);
            return String::from_utf16(&buffer).map_err(|_| refused("non-UTF-16 final path"));
        }
        // Too small: n is the required size including the terminator.
        buffer.resize(n + 1, 0);
    }
}

/// The normalized final path names exactly the requested path. This refuses
/// 8.3 short names and any other alias of a checked component.
fn same_path(final_path: &str, want: &Path) -> bool {
    let got = final_path.strip_prefix(r"\\?\").unwrap_or(final_path);
    let Some(want) = want.to_str() else {
        return false;
    };
    got.trim_end_matches('\\')
        .eq_ignore_ascii_case(want.trim_end_matches('\\'))
}

/// Check an opened handle: not a reparse point, the right type, no alias.
fn verify(
    handle: &impl AsRawHandle,
    path: &Path,
    dir: bool,
) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let info = info(handle)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(refused("reparse point (junction or symbolic link)"));
    }
    if dir != (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) {
        return Err(refused(if dir {
            "not a directory"
        } else {
            "not a regular file"
        }));
    }
    if !same_path(&final_path(handle)?, path) {
        return Err(refused("alias or short name"));
    }
    Ok(info)
}

/// Open an existing directory component itself and hold it against rename.
fn open_dir(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    let wide_path = wide(path.as_os_str())?;
    // SAFETY: NUL-terminated path. OPEN_REPARSE_POINT opens the component
    // itself, never a target. No FILE_SHARE_DELETE: no rename while held.
    let handle = owned(unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    })?;
    verify(&handle, path, true)?;
    Ok(handle)
}

/// Create a directory with an optional security descriptor. An existing
/// entry is not an error: the caller opens and checks it next.
fn create_dir(path: &Path, descriptor: Option<&Local>) -> io::Result<()> {
    let wide_path = wide(path.as_os_str())?;
    let attributes = descriptor.map(|d| SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: d.0,
        bInheritHandle: 0,
    });
    // SAFETY: NUL-terminated path; the optional descriptor stays alive
    // through the call. A null pointer inherits the parent DACL.
    let ok = unsafe {
        CreateDirectoryW(
            wide_path.as_ptr(),
            attributes
                .as_ref()
                .map_or(null(), |a| a as *const SECURITY_ATTRIBUTES),
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
    }
    Ok(())
}

fn agent_descriptor(account_sid: Option<&str>) -> io::Result<Local> {
    // Protected DACL: SYSTEM, Administrators, and the run-as account, all
    // inherited by files and directories below. Nothing from the parent.
    let mut sddl = String::from("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    if let Some(sid) = account_sid {
        if !sid.starts_with("S-1-") || !sid[4..].bytes().all(|b| b.is_ascii_digit() || b == b'-')
        {
            return Err(refused("invalid account SID"));
        }
        sddl.push_str(&format!("(A;OICI;FA;;;{sid})"));
    }
    let sddl = wide(OsStr::new(&sddl))?;
    let mut descriptor = null_mut();
    // SAFETY: NUL-terminated SDDL and writable LocalAlloc output.
    checked(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    })?;
    Ok(Local(descriptor))
}

/// The checked chain from `C:\` to one agent workspace. No handle leaves it.
pub struct Workspace {
    held: Vec<OwnedHandle>,
    root: PathBuf,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .finish()
    }
}

fn open_base(trusted: &[Local]) -> io::Result<Vec<OwnedHandle>> {
    let mut held = Vec::new();
    for ancestor in ANCESTORS {
        let handle = open_dir(Path::new(ancestor), DIR_ACCESS)?;
        audit_acl(&handle, trusted, trusted, &[], ANCESTOR_FORBIDDEN, false)
            .map_err(|_| refused("an ancestor of the workspace base is not protected"))?;
        held.push(handle);
    }
    let base = open_dir(Path::new(BASE), DIR_ACCESS)?;
    audit_acl(&base, trusted, trusted, &[], STRICT_FORBIDDEN, false)
        .map_err(|_| refused("the workspace base is not protected"))?;
    held.push(base);
    Ok(held)
}

impl Workspace {
    /// Is the base present and protected? Read only.
    pub fn base_ready() -> bool {
        trusted_sids().and_then(|t| open_base(&t)).is_ok()
    }

    /// Open (and with `create`, make) the workspace of `project`/`agent`.
    /// Run it with service authority, never while impersonating. The agent
    /// directory gets a protected DACL for SYSTEM, Administrators and the
    /// account of `identity`. An existing directory must grant exactly those
    /// trustees: one made for another account, or for service mode, is refused.
    pub fn open(
        project: &str,
        agent: &str,
        identity: Option<&Identity>,
        create: bool,
    ) -> io::Result<Self> {
        check_name(project)?;
        check_name(agent)?;
        let trusted = trusted_sids()?;
        let account_sid = identity.map(Identity::user_sid).transpose()?;
        let extra = account_sid
            .as_deref()
            .map(sid_from_string)
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        let mut held = open_base(&trusted)?;
        let project_path = Path::new(BASE).join(project);
        let project_dir = match open_dir(&project_path, DIR_ACCESS) {
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                create_dir(&project_path, None)?;
                open_dir(&project_path, DIR_ACCESS)?
            }
            other => other?,
        };
        audit_acl(&project_dir, &trusted, &trusted, &[], STRICT_FORBIDDEN, false)
            .map_err(|_| refused("the project directory is not protected"))?;
        held.push(project_dir);
        let root = project_path.join(agent);
        let agent_dir = match open_dir(&root, DIR_ACCESS) {
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                let descriptor = agent_descriptor(account_sid.as_deref())?;
                create_dir(&root, Some(&descriptor))?;
                open_dir(&root, DIR_ACCESS)?
            }
            other => other?,
        };
        audit_acl(&agent_dir, &trusted, &trusted, &extra, u32::MAX, true).map_err(|_| {
            refused("the workspace DACL is not protected or grants another account")
        })?;
        held.push(agent_dir);
        Ok(Self { held, root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Walk `rest` below the root and keep every component held. Run it
    /// under the account's impersonation so each open uses its rights.
    /// With `mkdirs`, a missing directory is created (it inherits the DACL).
    pub fn into_dir(self, rest: &[&str], mkdirs: bool) -> io::Result<Dir> {
        let Self { mut held, root } = self;
        let mut path = root;
        for name in rest {
            check_name(name)?;
            path.push(name);
            let handle = match open_dir(&path, INNER_ACCESS) {
                Err(e) if mkdirs && e.kind() == io::ErrorKind::NotFound => {
                    create_dir(&path, None)?;
                    open_dir(&path, INNER_ACCESS)?
                }
                other => other?,
            };
            held.push(handle);
        }
        Ok(Dir { held, path })
    }
}

/// A checked directory in a workspace, with its whole chain held.
pub struct Dir {
    held: Vec<OwnedHandle>,
    path: PathBuf,
}

impl std::fmt::Debug for Dir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dir")
            .field("path", &self.path)
            .field("held", &self.held.len())
            .finish()
    }
}

static TEMP: AtomicU64 = AtomicU64::new(0);

impl Dir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open a regular file of this directory for reading. Refuses a reparse
    /// point, a directory, an alias, and a file with more than one hard link.
    pub fn open_read(&self, name: &str) -> io::Result<File> {
        check_name(name)?;
        let path = self.path.join(name);
        let wide_path = wide(path.as_os_str())?;
        // SAFETY: NUL-terminated path; opens the entry itself, never a target.
        let handle = owned(unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN,
                null_mut(),
            )
        })?;
        let info = verify(&handle, &path, false)?;
        if info.nNumberOfLinks != 1 {
            return Err(refused("file has more than one hard link"));
        }
        Ok(File::from(handle))
    }

    /// A new temporary file in this directory (`CREATE_NEW`, never an
    /// existing entry). It is deleted by handle unless it is published.
    pub fn create_temp(&self, tag: u64) -> io::Result<TempFile> {
        let n = TEMP.fetch_add(1, Ordering::Relaxed);
        let path = self
            .path
            .join(format!(".terrarium-{tag}-{}-{n}.part", std::process::id()));
        let wide_path = wide(path.as_os_str())?;
        // SAFETY: NUL-terminated path. CREATE_NEW fails on any existing entry,
        // including a link. Share 0: nobody else opens it while it is written.
        let handle = owned(unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                0,
                null(),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        })?;
        let temp = TempFile {
            file: Some(File::from(handle)),
        };
        // On a failed check, Drop deletes the new file by its handle.
        verify(temp.file.as_ref().expect("live temporary file"), &path, false)?;
        Ok(temp)
    }
}

fn delete_on_close(file: File) {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: live handle opened with DELETE access; POD of the documented class.
    unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    drop(file);
}

/// A temporary file that is published by handle, or deleted by handle.
pub struct TempFile {
    file: Option<File>,
}

impl std::fmt::Debug for TempFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TempFile")
    }
}

impl TempFile {
    pub fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("live temporary file")
    }

    /// Flush and rename the file to `name` in its own directory, replacing a
    /// regular file of that name. `dir` must be the directory it was made in,
    /// still held. A directory of that name makes the rename fail.
    pub fn publish(mut self, dir: &Dir, name: &str) -> io::Result<()> {
        check_name(name)?;
        let _held = dir;
        let file = self.file.as_ref().expect("live temporary file");
        file.sync_all()?;
        let name: Vec<u16> = OsStr::new(name).encode_wide().collect();
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let bytes = offset + (name.len() + 1) * size_of::<u16>();
        let mut buffer = vec![0u64; bytes.div_ceil(size_of::<u64>())];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: the u64 buffer is aligned for FILE_RENAME_INFO and holds the
        // header plus the name and a terminator. A bare name with a null root
        // renames within the file's own directory; no path is resolved again.
        unsafe {
            (*info).Anonymous.ReplaceIfExists = true;
            (*info).RootDirectory = null_mut();
            (*info).FileNameLength = (name.len() * size_of::<u16>()) as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
                name.len(),
            );
        }
        // SAFETY: live handle with DELETE access and the buffer built above.
        checked(unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileRenameInfo,
                buffer.as_ptr().cast(),
                bytes as u32,
            )
        })?;
        drop(self.file.take());
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            delete_on_close(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_names_are_strict() {
        for good in ["src", "a.txt", "My File.bin", "x-y_z"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in [
            "", ".", "..", "a\\b", "a/b", "x:stream", "bad.", "bad ", "CON", "nul.txt", "com1",
            "LPT9.log", "é", "a*b", "a\nb",
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn final_path_comparison_refuses_aliases() {
        let want = Path::new(r"C:\Terrarium\work\p\a");
        assert!(same_path(r"\\?\C:\Terrarium\work\p\a", want));
        assert!(same_path(r"\\?\c:\terrarium\WORK\p\a", want));
        // A short-name request resolves to the long name: refused.
        assert!(!same_path(
            r"\\?\C:\Terrarium\work\p\a",
            Path::new(r"C:\TERRAR~1\work\p\a")
        ));
        assert!(!same_path(r"\\?\D:\Terrarium\work\p\a", want));
        assert!(!same_path(r"\\?\C:\Terrarium\work\p\other", want));
        assert!(same_path(r"\\?\C:\", Path::new(r"C:\")));
    }
}
