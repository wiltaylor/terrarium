//! Read trusted host policy without following reparse points or permitting
//! ancestor replacement while it is checked. This does not implement workspace ACLs.
use super::*;
use std::io::Read;
use std::path::Component;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE};

struct Local(*mut std::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: uniquely owned LocalAlloc-family allocation from security APIs.
        unsafe { LocalFree(self.0) };
    }
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Windows run-as requires a protected SYSTEM/Administrators-owned host config and ancestors",
    )
}

fn trusted_sid(sid: PSID, trusted: &[Local]) -> bool {
    if sid.is_null() {
        return false;
    }
    trusted.iter().any(|trusted| {
        // SAFETY: SID comes from a validated security descriptor or ACE, and
        // trusted SIDs come from successful ConvertStringSidToSid calls.
        unsafe { EqualSid(sid, trusted.0) != 0 }
    })
}

fn check_acl(handle: &OwnedHandle, file: bool, trusted: &[Local]) -> io::Result<()> {
    let (mut owner, mut dacl, mut descriptor) = (null_mut(), null_mut(), null_mut());
    // SAFETY: live read-control handle and writable pointer outputs. Descriptor
    // owns owner/DACL storage and remains alive through all ACE inspection.
    let error = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let _descriptor = Local(descriptor);
    if !trusted_sid(owner, if file { &trusted[..2] } else { trusted }) || dacl.is_null() {
        return Err(denied());
    }
    // Ancestors may allow creating unrelated children, but not deleting or
    // replacing this directory or changing its policy. On the file all write
    // permissions are forbidden for any untrusted trustee (conservative ACL audit).
    let forbidden = DELETE
        | WRITE_DAC
        | WRITE_OWNER
        | GENERIC_ALL
        | GENERIC_WRITE
        | if file {
            FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES
        } else {
            FILE_DELETE_CHILD
        };
    // SAFETY: GetSecurityInfo returned a valid ACL. GetAce bounds-checks index.
    for index in 0..unsafe { (*dacl).AceCount } as u32 {
        let mut ace = null_mut();
        // SAFETY: valid ACL and writable ACE pointer output.
        checked(unsafe { GetAce(dacl, index, &mut ace) })?;
        // SAFETY: GetAce returns an ACE_HEADER followed by the documented ACE body.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
            continue;
        }
        match header.AceType as u32 {
            ACCESS_ALLOWED_ACE_TYPE => {
                // SAFETY: type checked above, body contains a mask and SID start.
                let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                let sid = (&allowed.SidStart as *const u32).cast_mut().cast();
                if allowed.Mask & forbidden != 0 && !trusted_sid(sid, trusted) {
                    return Err(denied());
                }
            }
            ACCESS_DENIED_ACE_TYPE => {}
            // Unsupported conditional/object ACE semantics cannot silently grant authority.
            _ => return Err(denied()),
        }
    }
    Ok(())
}

/// Host policy must be on a local absolute drive path. Parent handles deny
/// delete sharing from root to leaf, so checked components cannot be renamed.
/// No ACL, ownership, account or privilege is changed by this function.
pub fn read_host_policy_file(path: &Path) -> io::Result<String> {
    let mut trusted = Vec::new();
    for sid in [
        "S-1-5-18",
        "S-1-5-32-544",
        // Windows Modules Installer service SID (TrustedInstaller).
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
    ] {
        let sid = wide(OsStr::new(sid))?;
        let mut output = null_mut();
        // SAFETY: NUL-terminated SID string and writable allocation output.
        checked(unsafe { ConvertStringSidToSidW(sid.as_ptr(), &mut output) })?;
        trusted.push(Local(output));
    }
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(denied());
    };
    if !matches!(prefix.kind(), std::path::Prefix::Disk(_))
        || components.next() != Some(Component::RootDir)
    {
        return Err(denied());
    }
    let mut current = std::path::PathBuf::from(prefix.as_os_str());
    current.push(r"\");
    let mut paths = vec![current.clone()];
    for component in components {
        let Component::Normal(name) = component else {
            return Err(denied());
        };
        // No ADS, Win32 trailing-dot aliases or ambiguous alternate spelling.
        let name_text = name.to_string_lossy();
        if name_text.contains(':') || name_text.ends_with(['.', ' ']) {
            return Err(denied());
        }
        current.push(name);
        paths.push(current.clone());
    }
    if paths.len() < 2 {
        return Err(denied());
    }
    let mut parents = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let file = index == paths.len() - 1;
        let path = wide(path.as_os_str())?;
        // SAFETY: checked path buffer. OPEN_REPARSE_POINT opens the component
        // itself, never its target. Previously opened parents remain locked.
        let handle = owned(unsafe {
            CreateFileW(
                path.as_ptr(),
                READ_CONTROL | FILE_READ_ATTRIBUTES | if file { FILE_READ_DATA } else { 0 },
                FILE_SHARE_READ,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            )
        })?;
        // SAFETY: live file handle and valid writable POD output.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
        checked(unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) })?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || file == (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0)
        {
            return Err(denied());
        }
        check_acl(&handle, file, &trusted)?;
        if file {
            let mut text = String::new();
            File::from(handle)
                .take(1024 * 1024 + 1)
                .read_to_string(&mut text)?;
            if text.len() > 1024 * 1024 {
                return Err(denied());
            }
            return Ok(text);
        }
        parents.push(handle);
    }
    Err(denied())
}
