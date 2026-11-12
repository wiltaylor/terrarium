//! Read-only token diagnostics for the native acceptance probe.
use super::*;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::*;

#[derive(Debug)]
pub struct TokenSnapshot {
    pub user_sid: String,
    pub elevation_type: u32,
    pub elevated: bool,
    pub administrators_enabled: bool,
    pub administrators_deny_only: bool,
}

fn information(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut size = 0;
    // SAFETY: documented buffer-size query, writable length output.
    unsafe { GetTokenInformation(token.as_raw_handle(), class, null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    // SAFETY: aligned allocation at least size bytes, class matches the sizing call.
    checked(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    })?;
    Ok(data)
}

/// Inspect the process token (not an impersonated thread token). This exports
/// only SID and policy bits, never token handles, credentials or environment.
pub fn current_token_snapshot() -> io::Result<TokenSnapshot> {
    let mut token = null_mut();
    // SAFETY: valid pseudo process handle and writable token output.
    checked(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })?;
    let token = owned(token)?;
    let user_data = information(&token, TokenUser)?;
    // SAFETY: successful TokenUser output contains an aligned TOKEN_USER and SID.
    let user = unsafe { &*user_data.as_ptr().cast::<TOKEN_USER>() };
    let mut string = null_mut();
    // SAFETY: valid SID in TokenUser buffer, writable LocalAlloc output.
    checked(unsafe { ConvertSidToStringSidW(user.User.Sid, &mut string) })?;
    struct SidString(*mut u16);
    impl Drop for SidString {
        fn drop(&mut self) {
            // SAFETY: uniquely owned string allocation from ConvertSidToStringSidW.
            unsafe { LocalFree(self.0.cast()) };
        }
    }
    let string = SidString(string);
    // SAFETY: successful conversion returns a NUL-terminated UTF-16 SID string.
    let user_sid = unsafe {
        let mut len = 0;
        while *string.0.add(len) != 0 {
            len += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(string.0, len))
            .map_err(|_| io::Error::other("invalid SID string"))?
    };
    let mut admin = [0u32; 17]; // SECURITY_MAX_SID_SIZE = 68, DWORD aligned.
    let mut size = std::mem::size_of_val(&admin) as u32;
    // SAFETY: sufficiently sized/aligned SID output and writable byte count.
    checked(unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            null_mut(),
            admin.as_mut_ptr().cast(),
            &mut size,
        )
    })?;
    let group_data = information(&token, TokenGroups)?;
    // SAFETY: OS-returned TokenGroups buffer contains the counted flexible array.
    let groups = unsafe { &*group_data.as_ptr().cast::<TOKEN_GROUPS>() };
    // SAFETY: successful GetTokenInformation allocated the entire group array.
    let groups =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    let attributes = groups
        .iter()
        .find(|group| {
            // SAFETY: both SIDs reside in live OS-filled buffers.
            unsafe { EqualSid(group.Sid, admin.as_ptr().cast_mut().cast()) != 0 }
        })
        .map_or(0, |group| group.Attributes);
    let (kind, elevated) = super::identity::elevation(&token)?;
    Ok(TokenSnapshot {
        user_sid,
        elevation_type: kind as u32,
        elevated,
        administrators_enabled: attributes & SE_GROUP_ENABLED != 0,
        administrators_deny_only: attributes & SE_GROUP_USE_FOR_DENY_ONLY != 0,
    })
}
