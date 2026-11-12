//! Password logon and strictly scoped synchronous impersonation.
//! No password, raw token or profile handle leaves this boundary.

use super::*;
use std::ffi::OsString;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::Environment::*;
use windows_sys::Win32::System::GroupPolicy::PI_NOUI;
use windows_sys::Win32::UI::Shell::{LoadUserProfileW, UnloadUserProfile, PROFILEINFOW};

/// One password logon. Keep this alive until the entire job has exited.
/// Debug deliberately omits account, environment and handles.
pub struct Identity {
    token: OwnedHandle,
    profile: HANDLE,
    environment: BTreeMap<String, String>,
}

// SAFETY: token/profile handles have no thread affinity. The environment is
// immutable and profile unloading occurs only on the final owner's drop.
unsafe impl Send for Identity {}
// SAFETY: shared token operations only read the token. Impersonation is per-thread.
unsafe impl Sync for Identity {}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Identity([redacted])")
    }
}

impl PartialEq for Identity {
    fn eq(&self, other: &Self) -> bool {
        self.token.as_raw_handle() == other.token.as_raw_handle()
    }
}
impl Eq for Identity {}

struct Password(Vec<u16>);
impl Drop for Password {
    fn drop(&mut self) {
        for c in &mut self.0 {
            // SAFETY: unique live allocation. Volatile stores prevent elision.
            unsafe { std::ptr::write_volatile(c, 0) };
        }
    }
}

fn elevation(token: &OwnedHandle) -> io::Result<(TOKEN_ELEVATION_TYPE, bool)> {
    let mut kind = 0;
    let mut elevated = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut bytes = 0;
    // SAFETY: live token and correctly sized POD outputs for both classes.
    checked(unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenElevationType,
            (&mut kind as *mut TOKEN_ELEVATION_TYPE).cast(),
            size_of::<TOKEN_ELEVATION_TYPE>() as u32, &mut bytes)
    })?;
    // SAFETY: live token and correctly sized POD output.
    checked(unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenElevation,
            (&mut elevated as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32, &mut bytes)
    })?;
    Ok((kind, elevated.TokenIsElevated != 0))
}

fn select_token(token: OwnedHandle, full: bool) -> io::Result<OwnedHandle> {
    let (kind, elevated) = elevation(&token)?;
    if (full && elevated) || (!full && !elevated && kind != TokenElevationTypeFull) {
        return Ok(token);
    }
    if (full && kind != TokenElevationTypeLimited)
        || (!full && kind != TokenElevationTypeFull)
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "requested Windows token is unavailable"));
    }
    let mut linked = TOKEN_LINKED_TOKEN { LinkedToken: null_mut() };
    let mut bytes = 0;
    // SAFETY: live token, POD output. Success transfers ownership of the linked handle.
    checked(unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenLinkedToken,
            (&mut linked as *mut TOKEN_LINKED_TOKEN).cast(),
            size_of::<TOKEN_LINKED_TOKEN>() as u32, &mut bytes)
    })?;
    let linked = owned(linked.LinkedToken)?;
    let (kind, elevated) = elevation(&linked)?;
    if (full && kind == TokenElevationTypeFull && elevated)
        || (!full && kind == TokenElevationTypeLimited && !elevated)
    {
        Ok(linked)
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "linked Windows token does not meet host policy"))
    }
}

impl Identity {
    /// `full` must come exclusively from trusted host configuration, not the wire.
    /// Accept DOMAIN\user, .\user, or a UPN. No NEW_CREDENTIALS identity substitute.
    pub fn logon(account: &str, password: &str, full: bool) -> io::Result<Self> {
        let (domain, user) = if let Some((domain, user)) = account.split_once('\\') {
            if domain.is_empty() || user.is_empty() || user.contains('\\') {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid Windows account"));
            }
            (Some(wide(OsStr::new(domain))?), user)
        } else if account.contains('@') {
            (None, account)
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "Windows account requires domain or local-machine qualifier"));
        };
        let mut user = wide(OsStr::new(user))?;
        let secret = Password(wide(OsStr::new(password))?);
        let mut token = null_mut();
        // SAFETY: NUL-terminated buffers survive call, writable handle output.
        // Logon errors contain OS codes only, never credentials.
        checked(unsafe {
            LogonUserW(user.as_ptr(), domain.as_ref().map_or(null(), |d| d.as_ptr()),
                secret.0.as_ptr(), LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT, &mut token)
        })?;
        drop(secret);
        let token = select_token(owned(token)?, full)?;
        // SAFETY: zero initialization is valid for PROFILEINFOW, then required fields set.
        let mut info: PROFILEINFOW = unsafe { zeroed() };
        info.dwSize = size_of::<PROFILEINFOW>() as u32;
        info.dwFlags = PI_NOUI;
        info.lpUserName = user.as_mut_ptr();
        // SAFETY: live primary token and valid profile-info buffers.
        checked(unsafe { LoadUserProfileW(token.as_raw_handle(), &mut info) })?;
        let mut identity = Self { token, profile: info.hProfile, environment: BTreeMap::new() };
        identity.environment = identity.read_environment()?;
        Ok(identity)
    }

    fn read_environment(&self) -> io::Result<BTreeMap<String, String>> {
        struct Block(*mut std::ffi::c_void);
        impl Drop for Block {
            fn drop(&mut self) {
                // SAFETY: uniquely owned successful CreateEnvironmentBlock allocation.
                unsafe { DestroyEnvironmentBlock(self.0) };
            }
        }
        let mut block = Block(null_mut());
        // SAFETY: live token, valid pointer output. FALSE forbids service-env inheritance.
        checked(unsafe { CreateEnvironmentBlock(&mut block.0, self.token.as_raw_handle(), 0) })?;
        let mut result = BTreeMap::new();
        let mut p = block.0.cast::<u16>();
        // SAFETY: CreateEnvironmentBlock returns a double-NUL terminated UTF-16 block.
        unsafe {
            while *p != 0 {
                let mut len = 0;
                while *p.add(len) != 0 { len += 1; }
                let line = String::from_utf16(std::slice::from_raw_parts(p, len))
                    .map_err(|_| io::Error::other("invalid user environment"))?;
                if let Some((key, value)) = line.split_once('=')
                    && !key.is_empty() {
                    result.insert(key.to_owned(), value.to_owned());
                }
                p = p.add(len + 1);
            }
        }
        Ok(result)
    }

    pub(crate) fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub(crate) fn token(&self) -> HANDLE {
        self.token.as_raw_handle()
    }

    /// Execute synchronous file operations on the current thread as this user.
    /// Never call async filesystem APIs here: their workers would use service authority.
    /// The closure returns only after impersonation has been reverted, including on panic.
    pub fn with_impersonation<T>(&self, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        struct Revert;
        impl Drop for Revert {
            fn drop(&mut self) {
                // SAFETY: runs on the same thread that installed impersonation.
                if unsafe { RevertToSelf() } == 0 {
                    // Returning a poisoned thread to any pool could leak authority.
                    std::process::abort();
                }
            }
        }
        // SAFETY: live logon token. Guard cannot cross an await or leave this stack.
        checked(unsafe { ImpersonateLoggedOnUser(self.token.as_raw_handle()) })?;
        let _guard = Revert;
        operation()
    }

    pub(crate) fn base_environment(&self) -> Vec<(OsString, OsString)> {
        self.environment.iter().map(|(k, v)| (k.into(), v.into())).collect()
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        // SAFETY: profile belongs to this logon and the token is still alive.
        // Callers must retain Identity through job drain. Failure does not retry
        // under another identity and never logs sensitive state.
        unsafe { UnloadUserProfile(self.token.as_raw_handle(), self.profile) };
    }
}
