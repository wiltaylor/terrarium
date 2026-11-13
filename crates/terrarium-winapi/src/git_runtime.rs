//! The trusted Git for Windows runtime and the session-private credential
//! directory of a managed checkout (TER-274 D10, D11).
//!
//! The runtime root comes only from the protected host file. Its ancestors
//! and every entry below it are opened without reparse following and
//! audited (bounded: [`MAX_ENTRIES`], [`MAX_DEPTH`]). A reparse point, an
//! alias or an unsupported name anywhere in the tree refuses the layout. No
//! untrusted trustee may change any directory or file of the runtime, so the
//! DLLs and support files that git, sh and ssh load are covered too. The
//! directories and the listed executables stay held for the whole checkout.
//!
//! Credential files live in `C:\Terrarium\git\<session>-<pid>-<n>`, with a
//! protected DACL for SYSTEM and the run-as account. They are created with
//! `FILE_FLAG_DELETE_ON_CLOSE`: the kernel deletes them when the host's
//! handle closes, on success, cancel, or a crash of the host.

use super::*;
use crate::policy_file::{Local, audit_acl, sid_from_string, trusted_sids};
use crate::workspace::{
    ANCESTOR_FORBIDDEN, ANCESTORS, DIR_ACCESS, INNER_ACCESS, STRICT_FORBIDDEN, check_name,
    dispose, open_dir, protected_descriptor, refused, verify,
};
use std::io::Write as _;
use std::path::PathBuf;
use windows_sys::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW};
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::Storage::FileSystem::*;

/// The parent of the session directories. The host creates it, protected.
pub const GIT_SESSIONS: &str = r"C:\Terrarium\git";

/// Untrusted trustees must not change a runtime binary.
const FILE_FORBIDDEN: u32 = DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_ALL
    | GENERIC_WRITE
    | FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES;

/// The most entries audited under a runtime root.
pub const MAX_ENTRIES: usize = 100_000;
/// The deepest directory level audited under a runtime root.
pub const MAX_DEPTH: usize = 32;

struct Find(HANDLE);
impl Drop for Find {
    fn drop(&mut self) {
        // SAFETY: a live find handle from FindFirstFileW, closed once.
        unsafe { FindClose(self.0) };
    }
}

/// The names in a directory (without `.` and `..`).
fn list(dir: &Path) -> io::Result<Vec<String>> {
    let pattern = wide(dir.join("*").as_os_str())?;
    // SAFETY: zero is a valid initial state of this POD output.
    let mut data: WIN32_FIND_DATAW = unsafe { zeroed() };
    // SAFETY: NUL-terminated pattern and a writable POD output.
    let handle = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let find = Find(handle);
    let mut names = Vec::new();
    loop {
        let len = data.cFileName.iter().position(|c| *c == 0).unwrap_or(260);
        let name = String::from_utf16(&data.cFileName[..len])
            .map_err(|_| refused("runtime file name is not UTF-16"))?;
        if name != "." && name != ".." {
            names.push(name);
        }
        // SAFETY: live find handle and writable POD output.
        if unsafe { FindNextFileW(find.0, &mut data) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok(names);
            }
            return Err(error);
        }
    }
}

/// Split a local drive path into its prefix (`C:\`) and checked names.
fn drive_components(path: &Path) -> io::Result<(PathBuf, Vec<String>)> {
    let text = path
        .to_str()
        .ok_or_else(|| refused("runtime path is not ASCII"))?;
    let bytes = text.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &text[1..3] != ":\\" {
        return Err(refused("runtime path must be a local drive path"));
    }
    let names: Vec<String> = text[3..]
        .split('\\')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    for name in &names {
        check_name(name)?;
    }
    Ok((PathBuf::from(&text[..3]), names))
}

/// The audited, held runtime files. Dropping it releases them.
pub struct TrustedFiles {
    _held: Vec<OwnedHandle>,
    root: PathBuf,
}

impl std::fmt::Debug for TrustedFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedFiles")
            .field("root", &self.root)
            .finish()
    }
}

impl TrustedFiles {
    /// Open and audit `root` and each of `files` (relative, `\`-separated).
    /// Read only: no ACL, owner or file is changed.
    pub fn open(root: &Path, files: &[&str]) -> io::Result<Self> {
        let trusted = trusted_sids()?;
        let (drive, names) = drive_components(root)?;
        if names.is_empty() {
            return Err(refused("runtime root is a drive root"));
        }
        let mut held = Vec::new();
        let mut path = drive;
        let handle = open_dir(&path, DIR_ACCESS)?;
        audit_acl(&handle, &trusted, &trusted, &[], ANCESTOR_FORBIDDEN, false)
            .map_err(|_| refused("a runtime ancestor is not protected"))?;
        held.push(handle);
        for (index, name) in names.iter().enumerate() {
            path.push(name);
            let handle = open_dir(&path, DIR_ACCESS)?;
            let forbidden = if index + 1 == names.len() {
                STRICT_FORBIDDEN
            } else {
                ANCESTOR_FORBIDDEN
            };
            audit_acl(&handle, &trusted, &trusted, &[], forbidden, false)
                .map_err(|_| refused("the runtime root or an ancestor is not protected"))?;
            held.push(handle);
        }
        // Every entry under the root: directories held, files audited.
        let mut stack = vec![(path.clone(), 0usize)];
        let mut entries = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            for name in list(&dir)? {
                entries += 1;
                if entries > MAX_ENTRIES {
                    return Err(refused("the runtime root has too many entries"));
                }
                check_name(&name).map_err(|_| refused("unsupported name in the runtime root"))?;
                let entry = dir.join(&name);
                let wide_path = wide(entry.as_os_str())?;
                // SAFETY: NUL-terminated path; opens the entry itself, never a
                // target. No write or delete sharing while it is checked.
                let handle = owned(unsafe {
                    CreateFileW(
                        wide_path.as_ptr(),
                        READ_CONTROL | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                        FILE_SHARE_READ,
                        null(),
                        OPEN_EXISTING,
                        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                        null_mut(),
                    )
                })?;
                let attributes = crate::workspace::info(&handle)?.dwFileAttributes;
                let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                verify(&handle, &entry, is_dir)?;
                let forbidden = if is_dir { STRICT_FORBIDDEN } else { FILE_FORBIDDEN };
                audit_acl(&handle, &trusted, &trusted, &[], forbidden, false)
                    .map_err(|_| refused("a runtime file or directory is not protected"))?;
                if is_dir {
                    if depth + 1 > MAX_DEPTH {
                        return Err(refused("the runtime root is too deep"));
                    }
                    held.push(handle);
                    stack.push((entry, depth + 1));
                }
            }
        }
        // The exact executables must exist as regular files, and stay held.
        for file in files {
            let mut current = path.clone();
            for part in file.split('\\') {
                check_name(part)?;
                current.push(part);
            }
            let wide_path = wide(current.as_os_str())?;
            // SAFETY: NUL-terminated path; opens the file itself, never a
            // target. Share read only: nobody can replace it while held.
            let handle = owned(unsafe {
                CreateFileW(
                    wide_path.as_ptr(),
                    READ_CONTROL | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                    FILE_SHARE_READ,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    null_mut(),
                )
            })?;
            verify(&handle, &current, false)?;
            held.push(handle);
        }
        Ok(Self {
            _held: held,
            root: path,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

static SESSION: AtomicU64 = AtomicU64::new(0);

/// A session-private directory. Subdirectories and the directory are
/// deleted by handle on drop (when empty). Drop its files first.
pub struct PrivateDir {
    _held: Vec<OwnedHandle>,
    dir: OwnedHandle,
    subdirs: Vec<OwnedHandle>,
    path: PathBuf,
}

impl std::fmt::Debug for PrivateDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateDir")
            .field("path", &self.path)
            .finish()
    }
}

/// Create `path` with `descriptor`; an existing entry is an error.
fn create_new_dir(path: &Path, descriptor: &Local) -> io::Result<()> {
    let wide_path = wide(path.as_os_str())?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: NUL-terminated path and a live descriptor through the call.
    checked(unsafe { CreateDirectoryW(wide_path.as_ptr(), &attributes) })
}

fn open_own_dir(path: &Path) -> io::Result<OwnedHandle> {
    open_dir(path, DIR_ACCESS | DELETE)
}

impl PrivateDir {
    /// Run with service authority, never while impersonating.
    pub fn create(tag: u64, identity: Option<&Identity>) -> io::Result<Self> {
        let trusted = trusted_sids()?;
        let mut held = Vec::new();
        for ancestor in ANCESTORS {
            let handle = open_dir(Path::new(ancestor), DIR_ACCESS)?;
            audit_acl(&handle, &trusted, &trusted, &[], ANCESTOR_FORBIDDEN, false)
                .map_err(|_| refused("an ancestor of the git session base is not protected"))?;
            held.push(handle);
        }
        let base = Path::new(GIT_SESSIONS);
        let handle = match open_dir(base, DIR_ACCESS) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let descriptor = protected_descriptor(&["SY", "BA"], None)?;
                match create_new_dir(base, &descriptor) {
                    Err(e) if e.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) => {}
                    other => other?,
                }
                open_dir(base, DIR_ACCESS)?
            }
            other => other?,
        };
        // Only trusted trustees, protected: a pre-created base is refused.
        audit_acl(&handle, &trusted, &trusted, &[], u32::MAX, true)
            .map_err(|_| refused("the git session base is not protected"))?;
        held.push(handle);
        let account_sid = identity.map(Identity::user_sid).transpose()?;
        let extra = account_sid
            .as_deref()
            .map(sid_from_string)
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        let n = SESSION.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("{tag}-{}-{n}", std::process::id()));
        // SYSTEM and the account only: OpenSSH refuses a key file that other
        // principals can read.
        let descriptor = protected_descriptor(&["SY"], account_sid.as_deref())?;
        create_new_dir(&path, &descriptor)?;
        let dir = open_own_dir(&path)?;
        let this = Self {
            _held: held,
            dir,
            subdirs: Vec::new(),
            path,
        };
        audit_acl(&this.dir, &trusted, &trusted[..1], &extra, u32::MAX, true)
            .map_err(|_| refused("the git session directory is not private"))?;
        Ok(this)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A new empty subdirectory, held until drop. Returns its path.
    pub fn subdir(&mut self, name: &str) -> io::Result<PathBuf> {
        check_name(name)?;
        let path = self.path.join(name);
        let wide_path = wide(path.as_os_str())?;
        // SAFETY: NUL-terminated path; null attributes inherit the private DACL.
        checked(unsafe { CreateDirectoryW(wide_path.as_ptr(), null()) })?;
        let handle = open_dir(&path, INNER_ACCESS | DELETE)?;
        self.subdirs.push(handle);
        Ok(path)
    }

    /// A new file with `data`, deleted by the kernel when the returned
    /// handle closes. Others may only read it (and must allow the delete).
    pub fn secret_file(&self, name: &str, data: &[u8]) -> io::Result<SecretFile> {
        check_name(name)?;
        let path = self.path.join(name);
        let wide_path = wide(path.as_os_str())?;
        // SAFETY: NUL-terminated path. CREATE_NEW refuses any existing entry.
        let handle = owned(unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                null(),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_DELETE_ON_CLOSE | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        })?;
        let mut file = File::from(handle);
        verify(&file, &path, false)?;
        file.write_all(data)?;
        file.flush()?;
        Ok(SecretFile { _file: file, path })
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        for handle in self.subdirs.drain(..).rev() {
            dispose(&handle);
        }
        dispose(&self.dir);
    }
}

/// A credential file that exists only while this value lives.
pub struct SecretFile {
    _file: File,
    path: PathBuf,
}

impl std::fmt::Debug for SecretFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretFile")
            .field("path", &self.path)
            .finish()
    }
}

impl SecretFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_paths_must_be_plain_drive_paths() {
        let (drive, names) = drive_components(Path::new(r"C:\Program Files\Git")).unwrap();
        assert_eq!(drive, PathBuf::from(r"C:\"));
        assert_eq!(names, ["Program Files", "Git"]);
        for bad in [
            r"\\server\share\Git",
            r"\\?\C:\Git",
            r"C:Git",
            r"C:\Git\..\x",
            r"C:\Gi:t",
            r"relative\Git",
        ] {
            assert!(drive_components(Path::new(bad)).is_err(), "{bad}");
        }
    }
}
