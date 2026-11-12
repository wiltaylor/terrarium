//! Windows-only, safe owned-handle boundary for atomic session containment.
//! No Win32 handles or unsafe functions are exposed to callers.
#![cfg(windows)]

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
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
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
pub struct Job(OwnedHandle);

impl Job {
    pub fn new(limits: Limits) -> io::Result<Self> {
        if limits.processes == Some(0) || limits.memory_bytes == Some(0)
            || limits.cpu_rate.is_some_and(|n| !(1..=10000).contains(&n)) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid job limit"));
        }
        // SAFETY: null security/name creates a fresh unnamed, non-inherited job.
        let job = Self(owned(unsafe { CreateJobObjectW(null(), null()) })?);
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
        checked(unsafe { SetInformationJobObject(job.0.as_raw_handle(), JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) })?;
        if let Some(rate) = limits.cpu_rate {
            let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: rate },
            };
            // SAFETY: live job and initialized CPU-rate POD with matching class/size.
            checked(unsafe { SetInformationJobObject(job.0.as_raw_handle(), JobObjectCpuRateControlInformation,
                (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(), size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32) })?;
        }
        Ok(job)
    }

    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: handle remains owned by self throughout the call.
        checked(unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) })
    }
}

/// Terminal dimensions, constrained to the signed COORD range.
#[derive(Clone, Copy)]
pub struct Size { pub cols: u16, pub rows: u16 }

fn coord(size: Size) -> io::Result<COORD> {
    if size.cols == 0 || size.rows == 0 || size.cols > i16::MAX as u16 || size.rows > i16::MAX as u16 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid terminal size"));
    }
    Ok(COORD { X: size.cols as i16, Y: size.rows as i16 })
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
        if hr < 0 { Err(io::Error::other(format!("ResizePseudoConsole: {hr:#x}"))) } else { Ok(()) }
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
    let attrs = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(), bInheritHandle: 1 };
    // SAFETY: writable handle outputs, valid optional security descriptor structure.
    checked(unsafe { CreatePipe(&mut read, &mut write, &attrs, 0) })?;
    Ok((owned(read)?, owned(write)?))
}

fn no_inherit(h: &OwnedHandle) -> io::Result<()> {
    // SAFETY: owned live kernel handle. Only its inheritance bit is changed.
    checked(unsafe { SetHandleInformation(h.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) })
}

struct Attributes { storage: Vec<usize>, initialized: bool }
impl Attributes {
    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: documented sizing call, null list, writable size output.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes) };
        if bytes == 0 { return Err(io::Error::last_os_error()); }
        let mut this = Self { storage: vec![0; bytes.div_ceil(size_of::<usize>())], initialized: false };
        // SAFETY: usize allocation is pointer-aligned, capacity at least the returned size.
        checked(unsafe { InitializeProcThreadAttributeList(this.ptr(), count, 0, &mut bytes) })?;
        this.initialized = true;
        Ok(this)
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST { self.storage.as_mut_ptr().cast() }
    fn add<T>(&mut self, key: usize, values: &[T]) -> io::Result<()> {
        // SAFETY: private callers pass the documented POD type for each key. The values
        // remain alive until CreateProcess returns, and Win32 only reads them.
        checked(unsafe { UpdateProcThreadAttribute(self.ptr(), 0, key, values.as_ptr().cast_mut().cast(),
            std::mem::size_of_val(values), null_mut(), null()) })
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
pub struct Process { handle: OwnedHandle, id: u32 }
impl Process {
    pub fn id(&self) -> u32 { self.id }
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
        if c == b'\\' as u16 { slashes += 1; continue; }
        out.extend(std::iter::repeat_n(b'\\' as u16, if c == b'"' as u16 { 2 * slashes + 1 } else { slashes }));
        out.push(c);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    out.push(b'"' as u16);
    Ok(out)
}

/// Create a service-account child atomically inside its job. The program
/// is explicit, not a shell command. Arguments follow Windows CRT quoting.
pub fn spawn(program: &Path, args: &[String], cwd: Option<&Path>, env: &BTreeMap<String, String>,
    terminal: Option<Size>, limits: Limits) -> io::Result<Child> {
    let job = Job::new(limits)?;
    let program = wide(program.as_os_str())?;
    let mut command = quote(OsStr::new(&String::from_utf16_lossy(&program[..program.len()-1])))?;
    for arg in args { command.push(32); command.extend(quote(OsStr::new(arg))?); }
    command.push(0);
    let cwd = cwd.map(|p| wide(p.as_os_str())).transpose()?;
    // Case-insensitive merge is required: PATH and Path are the same Windows key.
    let mut merged: BTreeMap<Vec<u16>, (std::ffi::OsString, std::ffi::OsString)> = BTreeMap::new();
    for (key, value) in std::env::vars_os().chain(env.iter().map(|(k,v)| (k.into(),v.into()))) {
        if key.is_empty() || key.to_string_lossy().contains('=') { continue; }
        merged.insert(key.to_string_lossy().to_uppercase().encode_utf16().collect(), (key, value));
    }
    let mut environment = Vec::new();
    for (_, (k,v)) in merged {
        let k = wide(&k)?;
        environment.extend_from_slice(&k[..k.len()-1]); environment.push(61);
        environment.extend(wide(&v)?);
    }
    environment.push(0);
    if environment.len() == 1 { environment.push(0); }
    let (child_in, parent_in) = pipe()?;
    let (parent_out, child_out) = pipe()?;
    no_inherit(&parent_in)?;
    no_inherit(&parent_out)?;
    let (mut parent_err, mut child_err) = (None, None);
    let console = if let Some(size) = terminal {
        let mut handle = 0;
        // SAFETY: valid pipe handles, checked dimensions, writable HPCON output.
        // flags=0 deliberately avoids cursor inheritance from the service's absent console.
        let hr = unsafe { CreatePseudoConsole(coord(size)?, child_in.as_raw_handle(), child_out.as_raw_handle(), 0, &mut handle) };
        if hr < 0 { return Err(io::Error::other(format!("CreatePseudoConsole: {hr:#x}"))); }
        Some(Console(handle))
    } else {
        let (read, write) = pipe()?;
        no_inherit(&read)?;
        parent_err = Some(read);
        child_err = Some(write);
        None
    };
    let mut attributes = Attributes::new(2)?;
    let jobs = [job.0.as_raw_handle()];
    attributes.add(PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, &jobs)?;
    let handles = [child_in.as_raw_handle(), child_out.as_raw_handle(), child_err.as_ref().map_or(null_mut(), |h| h.as_raw_handle())];
    if let Some(console) = &console {
        // PSEUDOCONSOLE is the value itself, not a pointer to an HPCON. The
        // generic array helper is not applicable to this one attribute.
        // SAFETY: live HPCON is held until child creation and for the child's lifetime.
        checked(unsafe { UpdateProcThreadAttribute(attributes.ptr(), 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            console.0 as *mut _, size_of::<HPCON>(), null_mut(), null()) })?;
    } else {
        attributes.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, &handles)?;
    }
    // SAFETY: both Win32 structures are POD with a valid zero initial state.
    let (mut startup, mut result): (STARTUPINFOEXW, PROCESS_INFORMATION) = unsafe { (zeroed(), zeroed()) };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes.ptr();
    if console.is_none() {
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
    }
    let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT
        | if console.is_none() { CREATE_NO_WINDOW } else { 0 };
    // SAFETY: all inputs point to initialized buffers alive through this call.
    // JOB_LIST assigns containment before any thread runs. HANDLE_LIST limits
    // inheritance to the three pipe ends; ConPTY children inherit no handles.
    checked(unsafe { CreateProcessW(program.as_ptr(), command.as_mut_ptr(), null(), null(),
        i32::from(console.is_none()), flags, environment.as_ptr().cast(),
        cwd.as_ref().map_or(null(), |s| s.as_ptr()), &startup.StartupInfo, &mut result) })?;
    let process = Process { handle: owned(result.hProcess)?, id: result.dwProcessId };
    drop(owned(result.hThread)?);
    Ok(Child { job, process, console, stdin: parent_in.into(), stdout: parent_out.into(), stderr: parent_err.map(File::from) })
}
