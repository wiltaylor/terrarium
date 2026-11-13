//! Windows-only, safe owned-handle boundary for atomic session containment.
//! No Win32 handles or unsafe functions are exposed to callers.
#![cfg(windows)]

mod identity;
pub use identity::Identity;
mod policy_file;
pub use policy_file::read_host_policy_file;
mod diagnostics;
mod workspace;
pub use workspace::{BASE as WORKSPACE_BASE, Dir, Entry, TempFile, Workspace, check_name};
mod git_runtime;
pub use git_runtime::{GIT_SESSIONS, PrivateDir, SecretFile, TrustedFiles};
pub use diagnostics::{TokenSnapshot, current_token_snapshot};

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::*;

fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
    let mut v: Vec<_> = s.encode_wide().collect();
    if v.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "embedded NUL"));
    }
    v.push(0);
    Ok(v)
}

fn checked(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: caller supplies a fresh successful Win32 handle, never borrowed.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

/// Hard limits for the entire process tree. CPU rate is 1..=10000 (1/100 %).
#[derive(Clone, Copy, Debug, Default)]
pub struct Limits {
    pub memory_bytes: Option<usize>,
    pub processes: Option<u32>,
    pub cpu_rate: Option<u32>,
}

/// One session's job. The last handle closing kills every descendant.
#[derive(Debug)]
pub struct Job(
    OwnedHandle,
    std::sync::Mutex<bool>,
    Option<std::sync::Arc<Identity>>,
);

impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_raw_handle() == other.0.as_raw_handle()
    }
}
impl Eq for Job {}

impl Job {
    pub fn new(limits: Limits) -> io::Result<Self> {
        // SAFETY: null security/name creates a fresh unnamed, non-inherited job.
        let job = Self(
            owned(unsafe { CreateJobObjectW(null(), null()) })?,
            std::sync::Mutex::new(false),
            None,
        );
        job.set_limits(limits)?;
        Ok(job)
    }

    /// Update the shared holder budget without replacing its job handle.
    pub fn set_limits(&self, limits: Limits) -> io::Result<()> {
        let mut cpu_enabled = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if limits.processes == Some(0)
            || limits.memory_bytes == Some(0)
            || limits.cpu_rate.is_some_and(|n| !(1..=10000).contains(&n))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid job limit",
            ));
        }
        // SAFETY: this Win32 POD has a valid all-zero initial state.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(n) = limits.memory_bytes {
            info.JobMemoryLimit = n;
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        }
        if let Some(n) = limits.processes {
            info.BasicLimitInformation.ActiveProcessLimit = n;
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        }
        // SAFETY: live job and initialized info of the documented class and size.
        checked(unsafe {
            SetInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        })?;
        if limits.cpu_rate.is_some() || *cpu_enabled {
            let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                ControlFlags: if limits.cpu_rate.is_some() {
                    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP
                } else {
                    0
                },
                Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 {
                    CpuRate: limits.cpu_rate.unwrap_or(0),
                },
            };
            // SAFETY: live job and initialized CPU-rate POD with matching class/size.
            checked(unsafe {
                SetInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectCpuRateControlInformation,
                    (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                    size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                )
            })?;
            *cpu_enabled = limits.cpu_rate.is_some();
        }
        Ok(())
    }

    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: handle remains owned by self throughout the call.
        checked(unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) })
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let Some(identity) = self.2.take() else {
            return;
        };
        // A profile must not unload while a descendant still uses it. Explicit
        // termination precedes closing this last session job handle, even on
        // async cancellation. If Windows cannot confirm drain, retain the logon
        // rather than unload a live profile. This bounded failure leaks no secret.
        let _ = self.terminate();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // SAFETY: valid zero-initialized POD and owned job handle.
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            // SAFETY: live job and output matching class and documented size.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                )
            };
            if ok != 0 && info.ActiveProcesses == 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                std::mem::forget(identity);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Terminal dimensions, constrained to the signed COORD range.
#[derive(Clone, Copy)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

fn coord(size: Size) -> io::Result<COORD> {
    if size.cols == 0
        || size.rows == 0
        || size.cols > i16::MAX as u16
        || size.rows > i16::MAX as u16
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid terminal size",
        ));
    }
    Ok(COORD {
        X: size.cols as i16,
        Y: size.rows as i16,
    })
}

/// A pseudoconsole. Closing it is separate from closing the process tree.
pub struct Console(HPCON);
// SAFETY: HPCON has no thread affinity. Methods borrow it, and Drop has exclusive ownership.
unsafe impl Send for Console {}
// SAFETY: ResizePseudoConsole supports concurrent callers; no Rust memory is shared with Win32.
unsafe impl Sync for Console {}

impl Console {
    pub fn resize(&self, size: Size) -> io::Result<()> {
        let size = coord(size)?;
        // SAFETY: self owns a live pseudoconsole and size was checked.
        let hr = unsafe { ResizePseudoConsole(self.0, size) };
        if hr < 0 {
            Err(io::Error::other(format!("ResizePseudoConsole: {hr:#x}")))
        } else {
            Ok(())
        }
    }
}
impl Drop for Console {
    fn drop(&mut self) {
        // SAFETY: unique owner, closed exactly once, not a kernel CloseHandle handle.
        unsafe { ClosePseudoConsole(self.0) };
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write) = (null_mut(), null_mut());
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: writable handle outputs, valid optional security descriptor structure.
    checked(unsafe { CreatePipe(&mut read, &mut write, &attrs, 0) })?;
    Ok((owned(read)?, owned(write)?))
}

fn no_inherit(h: &OwnedHandle) -> io::Result<()> {
    // SAFETY: owned live kernel handle. Only its inheritance bit is changed.
    checked(unsafe { SetHandleInformation(h.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) })
}

struct Attributes {
    storage: Vec<usize>,
    initialized: bool,
}
impl Attributes {
    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: documented sizing call, null list, writable size output.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut this = Self {
            storage: vec![0; bytes.div_ceil(size_of::<usize>())],
            initialized: false,
        };
        // SAFETY: usize allocation is pointer-aligned, capacity at least the returned size.
        checked(unsafe { InitializeProcThreadAttributeList(this.ptr(), count, 0, &mut bytes) })?;
        this.initialized = true;
        Ok(this)
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
    fn add<T>(&mut self, key: usize, values: &[T]) -> io::Result<()> {
        // SAFETY: private callers pass the documented POD type for each key. The values
        // remain alive until CreateProcess returns, and Win32 only reads them.
        checked(unsafe {
            UpdateProcThreadAttribute(
                self.ptr(),
                0,
                key,
                values.as_ptr().cast_mut().cast(),
                std::mem::size_of_val(values),
                null_mut(),
                null(),
            )
        })
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: initialized list stays allocated until after this deletion.
            unsafe { DeleteProcThreadAttributeList(self.ptr()) };
        }
    }
}

/// Process handle, independent of recycled process IDs.
pub struct Process {
    handle: OwnedHandle,
    id: u32,
}
impl Process {
    pub fn id(&self) -> u32 {
        self.id
    }
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        // SAFETY: valid owned process handle, zero timeout does not block.
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: live process handle and writable exit-code output.
                checked(unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) })?;
                Ok(Some(code))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// A contained child and its parent-side I/O. Dropping job kills the tree.
pub struct Child {
    pub job: Job,
    pub process: Process,
    pub console: Option<Console>,
    pub stdin: File,
    pub stdout: File,
    pub stderr: Option<File>,
}

// Windows CRT command-line quoting, including trailing backslashes in quotes.
fn quote(arg: &OsStr) -> io::Result<Vec<u16>> {
    let arg = wide(arg)?;
    let mut out = vec![b'"' as u16];
    let mut slashes = 0;
    for &c in &arg[..arg.len() - 1] {
        if c == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            b'\\' as u16,
            if c == b'"' as u16 {
                2 * slashes + 1
            } else {
                slashes
            },
        ));
        out.push(c);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    out.push(b'"' as u16);
    Ok(out)
}

/// Create a service-account child atomically inside its job. The program
/// is explicit, not a shell command. Arguments follow Windows CRT quoting.
pub fn spawn(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    terminal: Option<Size>,
    limits: Limits,
) -> io::Result<Child> {
    spawn_in_holder(program, args, cwd, env, terminal, limits, None)
}

/// Nested jobs keep a shared holder budget and an independent killable
/// session tree. The parent must outlive every session using its budget.
pub fn spawn_in_holder(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    terminal: Option<Size>,
    limits: Limits,
    holder: Option<&Job>,
) -> io::Result<Child> {
    spawn_with_context(
        program,
        args,
        cwd,
        env,
        terminal,
        limits,
        SpawnContext {
            holder,
            ..SpawnContext::default()
        },
    )
}

/// Trusted local identity and shared holder, never deserialized from the wire.
#[derive(Default)]
pub struct SpawnContext<'a> {
    pub holder: Option<&'a Job>,
    pub identity: Option<&'a std::sync::Arc<Identity>>,
    /// Names removed from the child environment (case-insensitive), after
    /// `env` is merged. For the managed checkout's `GIT_*` list (TER-274).
    pub remove_env: &'a [&'a str],
}

/// CreateProcessAsUser keeps the exact same atomic job/handle/ConPTY boundary
/// as the service-account spawn. Failure never falls back to CreateProcessW.
pub fn spawn_with_context(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    terminal: Option<Size>,
    limits: Limits,
    context: SpawnContext<'_>,
) -> io::Result<Child> {
    let SpawnContext {
        holder,
        identity,
        remove_env,
    } = context;
    let mut job = Job::new(limits)?;
    job.2 = identity.cloned();
    let user_cwd = identity
        .map(|identity| {
            identity
                .environment()
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("USERPROFILE"))
                .map(|(_, v)| std::path::PathBuf::from(v))
                .filter(|p| p.is_absolute())
                .ok_or_else(|| io::Error::other("user profile directory is unavailable"))
        })
        .transpose()?;
    let cwd = cwd.or(user_cwd.as_deref());
    let mut search_env = identity
        .map(|i| i.environment().clone())
        .unwrap_or_default();
    if identity.is_some() && !search_env.keys().any(|k| k.eq_ignore_ascii_case("PATH")) {
        search_env.insert("PATH".into(), String::new());
    }
    for (key, value) in env {
        search_env.retain(|k, _| !k.eq_ignore_ascii_case(key));
        search_env.insert(key.clone(), value.clone());
    }
    let executable = match identity {
        Some(identity) => {
            identity.with_impersonation(|| resolve_program(program, cwd, &search_env))?
        }
        None => resolve_program(program, cwd, env)?,
    };
    let program = wide(executable.as_os_str())?;
    let mut command = quote(executable.as_os_str())?;
    for arg in args {
        command.push(32);
        command.extend(quote(OsStr::new(arg))?);
    }
    command.push(0);
    let cwd = cwd.map(|p| wide(p.as_os_str())).transpose()?;
    // Case-insensitive merge is required: PATH and Path are the same Windows key.
    let mut merged: BTreeMap<Vec<u16>, (std::ffi::OsString, std::ffi::OsString)> = BTreeMap::new();
    let base = identity.map_or_else(|| std::env::vars_os().collect(), |i| i.base_environment());
    for (key, value) in base
        .into_iter()
        .chain(env.iter().map(|(k, v)| (k.into(), v.into())))
    {
        if key.is_empty() || key.to_string_lossy().contains('=') {
            continue;
        }
        merged.insert(
            key.to_string_lossy()
                .to_uppercase()
                .encode_utf16()
                .collect(),
            (key, value),
        );
    }
    for name in remove_env {
        merged.remove(&name.to_uppercase().encode_utf16().collect::<Vec<u16>>());
    }
    let mut environment = Vec::new();
    for (_, (k, v)) in merged {
        let k = wide(&k)?;
        environment.extend_from_slice(&k[..k.len() - 1]);
        environment.push(61);
        environment.extend(wide(&v)?);
    }
    environment.push(0);
    if environment.len() == 1 {
        environment.push(0);
    }
    let (child_in, parent_in) = pipe()?;
    let (parent_out, child_out) = pipe()?;
    no_inherit(&parent_in)?;
    no_inherit(&parent_out)?;
    let (mut parent_err, mut child_err) = (None, None);
    let console = if let Some(size) = terminal {
        let mut handle = 0;
        // SAFETY: valid pipe handles, checked dimensions, writable HPCON output.
        // flags=0 deliberately avoids cursor inheritance from the service's absent console.
        let hr = unsafe {
            CreatePseudoConsole(
                coord(size)?,
                child_in.as_raw_handle(),
                child_out.as_raw_handle(),
                0,
                &mut handle,
            )
        };
        if hr < 0 {
            return Err(io::Error::other(format!("CreatePseudoConsole: {hr:#x}")));
        }
        Some(Console(handle))
    } else {
        let (read, write) = pipe()?;
        no_inherit(&read)?;
        parent_err = Some(read);
        child_err = Some(write);
        None
    };
    let mut attributes = Attributes::new(2)?;
    let mut jobs = Vec::with_capacity(2);
    if let Some(holder) = holder {
        jobs.push(holder.0.as_raw_handle());
    }
    jobs.push(job.0.as_raw_handle());
    attributes.add(PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, &jobs)?;
    let handles = [
        child_in.as_raw_handle(),
        child_out.as_raw_handle(),
        child_err.as_ref().map_or(null_mut(), |h| h.as_raw_handle()),
    ];
    if let Some(console) = &console {
        // PSEUDOCONSOLE is the value itself, not a pointer to an HPCON. The
        // generic array helper is not applicable to this one attribute.
        // SAFETY: live HPCON is held until child creation and for the child's lifetime.
        checked(unsafe {
            UpdateProcThreadAttribute(
                attributes.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console.0 as *mut _,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            )
        })?;
    } else {
        attributes.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, &handles)?;
    }
    // SAFETY: both Win32 structures are POD with a valid zero initial state.
    let (mut startup, mut result): (STARTUPINFOEXW, PROCESS_INFORMATION) =
        unsafe { (zeroed(), zeroed()) };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes.ptr();
    // NULL inherits the parent's station/desktop. A non-NULL empty name asks
    // Windows to select/connect the logon session's default instead. Do not
    // select winsta0 or grant access to the service's existing desktop.
    // https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw
    let mut desktop = desktop_name(identity.is_some());
    startup.StartupInfo.lpDesktop = desktop
        .as_mut()
        .map_or(null_mut(), |name| name.as_mut_ptr());
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    // Explicit invalid handles prevent Windows from copying the service's
    // redirected standard handles instead of connecting the pseudoconsole.
    startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    if console.is_none() {
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
    }
    let flags = EXTENDED_STARTUPINFO_PRESENT
        | CREATE_UNICODE_ENVIRONMENT
        | if console.is_none() {
            CREATE_NO_WINDOW
        } else {
            0
        };
    // SAFETY: all inputs point to initialized buffers alive through this call.
    // JOB_LIST assigns containment before any thread runs. HANDLE_LIST limits
    // inheritance to the three pipe ends; ConPTY children inherit no handles.
    if let Some(identity) = identity {
        // Windows otherwise opens the image/cwd as the service even though
        // the new process has a user token. Scope those opens to the user too.
        // CreateProcessAsUser's documented remarks explicitly recommend this
        // impersonation sequence. An earlier access check followed by a
        // service-authority image open is not an equivalent security boundary.
        identity.with_impersonation(|| {
            checked(unsafe {
                CreateProcessAsUserW(
                    identity.token(),
                    program.as_ptr(),
                    command.as_mut_ptr(),
                    null(),
                    null(),
                    i32::from(console.is_none()),
                    flags,
                    environment.as_ptr().cast(),
                    cwd.as_ref().map_or(null(), |s| s.as_ptr()),
                    &startup.StartupInfo,
                    &mut result,
                )
            })
        })?;
    } else {
        // SAFETY: same initialized buffers and atomic containment as above.
        checked(unsafe {
            CreateProcessW(
                program.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                i32::from(console.is_none()),
                flags,
                environment.as_ptr().cast(),
                cwd.as_ref().map_or(null(), |s| s.as_ptr()),
                &startup.StartupInfo,
                &mut result,
            )
        })?;
    }
    let process = Process {
        handle: owned(result.hProcess)?,
        id: result.dwProcessId,
    };
    drop(owned(result.hThread)?);
    Ok(Child {
        job,
        process,
        console,
        stdin: parent_in.into(),
        stdout: parent_out.into(),
        stderr: parent_err.map(File::from),
    })
}

fn desktop_name(run_as: bool) -> Option<[u16; 1]> {
    run_as.then_some([0])
}

#[cfg(test)]
mod desktop_tests {
    use super::desktop_name;

    #[test]
    fn run_as_selects_empty_name_instead_of_parent_desktop_inheritance() {
        assert_eq!(desktop_name(true), Some([0]));
        assert_eq!(desktop_name(false), None);
    }
}

fn resolve_program(
    program: &Path,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> io::Result<std::path::PathBuf> {
    let base = cwd
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);
    let candidates = if program.components().count() > 1 || program.is_absolute() {
        vec![base.join(program)]
    } else {
        let path = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| std::ffi::OsString::from(value))
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| base.join(dir).join(program))
            .collect()
    };
    for mut candidate in candidates {
        // CreateProcess executes images, not shell scripts or PATHEXT handlers.
        if candidate.extension().is_none() {
            candidate.set_extension("exe");
        }
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("executable not found: {}", program.display()),
    ))
}
