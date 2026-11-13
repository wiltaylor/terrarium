//! The script step of the board rules (TER-173, TER-162 D1): a wscript
//! program ([`rules_file::SCRIPT_VERB`]) for what WCL cannot say.
//!
//! The script is untrusted code (any project admin writes a `rules.wcl`),
//! so the daemon never runs it in its own process:
//!
//! - **A child process.** [`run`] starts the daemon's own binary with
//!   [`CHILD_ARG`] ([`child_main`]): an empty environment, `/` as its
//!   directory, the script and its view on stdin as JSON, the result on
//!   stdout. The child sets its own rlimits before it reads the script:
//!   [`MEMORY_BYTES`] of data (`RLIMIT_DATA`, so a string that doubles in
//!   a loop ends the child, not the daemon), [`CPU_SECONDS`] of CPU, no
//!   file writes (`RLIMIT_FSIZE` 0), no core file, and a few open files.
//! - **No host access.** The wscript stdlib is not compiled in (no
//!   default features): the script has no fs, process, json or env
//!   module. The only module is `rule`: `rule::get(name)` reads one value
//!   of the view, and `rule::quote(text)` writes a WCL string. `print`
//!   writes nowhere. `compile` resolves no import file.
//! - **Bounds.** Fuel ([`Vm::set_fuel`], the step's `fuel`, at most
//!   [`rules_file::MAX_SCRIPT_FUEL`]), a call depth of [`CALL_DEPTH`],
//!   an input of at most [`MAX_INPUT`] bytes, an output of at most
//!   [`rules_file::MAX_SCRIPT_OUTPUT`] bytes, [`WALL_CLOCK`] from start
//!   to result (then the child is killed). The engine's runs share
//!   [`MAX_RUNNING`] slots and wait at most [`ENGINE_ACQUIRE`] for one.
//!   The dry runs have their own [`DRY_RUN_SLOTS`] (wait at most
//!   [`DRY_RUN_ACQUIRE`], at most [`DRY_RUN_TOTAL`] for one dry run, the
//!   waits and the runs under one deadline), so a dry run never holds a
//!   slot of the engine. One project holds at most its share of a pool
//!   ([`PROJECT_ENGINE_SLOTS`], [`PROJECT_DRY_RUN_SLOTS`]): it waits for
//!   its share first, in its own queue, so one project's slow scripts
//!   do not keep the other projects out (TER-551).
//! - **seccomp.** After its rlimits the child sets `no_new_privs` and a
//!   seccomp filter ([`confine`], TER-545): it may read its stdin, write
//!   its stdout and stderr, manage its memory, and exit. Each other call
//!   (an open, a socket, an exec) fails with `EPERM`.
//! - **No inherited files.** The child closes each file from 3 up before
//!   it reads its input, so an fd of the daemon without close-on-exec
//!   does not reach the script.
//! - **No database lock.** The engine takes the view in a write, then
//!   runs the script after that write, then checks and records the
//!   returned verbs in the next write (`crate::rules_engine`). The dry run
//!   runs its scripts after its read.
//!
//! The script returns verb blocks in the syntax of a rules file; the
//! engine reads them with [`rules_file::script_actions`] and runs them in
//! place of the step, as the rule actor, with the store's checks.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use terrarium_core::rules_file;
use terrarium_core::rules_tools;
use terrarium_core::template::{EventView, value_text};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wscript::{Context, Module, Vm, VmConfig};

use crate::rules_engine::{RunCtx, lookup};

/// The first argument of the daemon binary that makes it a script child.
pub const CHILD_ARG: &str = "__rule-script";
/// The data limit of a child (`RLIMIT_DATA`).
pub const MEMORY_BYTES: u64 = 256 * 1024 * 1024;
/// The CPU limit of a child (`RLIMIT_CPU`).
pub const CPU_SECONDS: u64 = 10;
/// The time from the start of a child to its result.
pub const WALL_CLOCK: Duration = Duration::from_secs(5);
/// The most bytes of the input of a child: the source and the view.
pub const MAX_INPUT: usize = 256 * 1024;
/// The script call-depth limit.
pub const CALL_DEPTH: usize = 200;
/// The most children of the engine's runs at a time.
pub const MAX_RUNNING: usize = 4;
/// The longest that a run waits for one of the [`MAX_RUNNING`] slots;
/// then its step fails ("the script slots are busy"), so a lane never
/// waits without limit.
pub const ENGINE_ACQUIRE: Duration = Duration::from_secs(30);
/// The children of the dry runs at a time: their own pool, so a dry run
/// never takes a slot of the engine.
pub const DRY_RUN_SLOTS: usize = 1;
/// The most engine slots that one project holds at a time.
pub const PROJECT_ENGINE_SLOTS: usize = 2;
/// The most dry-run slots that one project holds at a time.
pub const PROJECT_DRY_RUN_SLOTS: usize = 1;
/// The longest that a dry run waits for its slot; then it is refused.
pub const DRY_RUN_ACQUIRE: Duration = Duration::from_secs(2);
/// The most time of all the scripts of one dry run, waits included.
pub const DRY_RUN_TOTAL: Duration = Duration::from_secs(15);
/// The most open files of a child (stdin, stdout, stderr, and the few
/// that the C library may need).
const MAX_FILES: u64 = 16;
/// The most bytes that the parent reads of the child's stdout.
const MAX_REPLY: usize = 2 * rules_file::MAX_SCRIPT_OUTPUT + 4096;

/// The placeholders of the view besides `event.*` (which the child reads
/// from the event).
fn view_names() -> impl Iterator<Item = String> {
    let card = rules_file::CARD_FIELDS.iter().map(|f| format!("card.{f}"));
    let group = rules_file::GROUP_FIELDS
        .iter()
        .map(|f| format!("group.{f}"));
    let job = rules_file::JOB_FIELDS.iter().map(|f| format!("job.{f}"));
    card.chain(group)
        .chain(job)
        .chain(std::iter::once("rule.name".to_owned()))
}

/// The event of the view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventInput {
    pub seq: i64,
    pub kind: String,
    pub summary: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub data: Value,
}

/// What a child gets: the script and its read-only view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub source: String,
    pub fuel: u64,
    /// The values of `card.*`, `group.*`, `job.*` and `rule.name` that
    /// have one.
    pub values: BTreeMap<String, String>,
    pub event: Option<EventInput>,
}

impl Input {
    /// The input of script step `action` of a run with `ctx`. The values
    /// are the ones that a template of the step would read now.
    pub fn new(action: &rules_file::Action, ctx: &RunCtx) -> Input {
        let source = action
            .args
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let fuel = action
            .args
            .get("fuel")
            .and_then(Value::as_u64)
            .unwrap_or(rules_file::DEFAULT_SCRIPT_FUEL)
            .min(rules_file::MAX_SCRIPT_FUEL);
        let values = view_names()
            .filter_map(|n| lookup(ctx, &n).map(|v| (n, v)))
            .collect();
        let event = ctx.event.as_ref().map(|e| EventInput {
            seq: e.seq,
            kind: e.kind.clone(),
            summary: e.summary.clone(),
            project: e.project.clone(),
            actor: e.actor.clone(),
            data: e.data.clone(),
        });
        Input {
            source,
            fuel,
            values,
            event,
        }
    }

    /// The value of placeholder `name`, as `rule::get` reads it.
    pub fn get(&self, name: &str) -> Option<String> {
        if let Some(path) = name.strip_prefix("event.") {
            let e = self.event.as_ref()?;
            let view = EventView {
                seq: e.seq,
                kind: &e.kind,
                summary: &e.summary,
                project: e.project.as_deref(),
                actor: e.actor.as_deref(),
                data: &e.data,
            };
            return view.get(path).map(|v| value_text(&v));
        }
        self.values.get(name).cloned()
    }
}

/// What a script did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// The verb blocks that `main` returned.
    #[serde(default)]
    pub output: Option<String>,
    /// Why the script failed (compile, runtime, fuel, limits).
    #[serde(default)]
    pub error: Option<String>,
    /// The fuel that the script used.
    #[serde(default)]
    pub fuel_used: u64,
}

impl Reply {
    fn failed(error: impl Into<String>) -> Reply {
        Reply {
            output: None,
            error: Some(error.into()),
            fuel_used: 0,
        }
    }
}

/// The line (1-based) of byte `offset` of `source`.
fn line_of(source: &str, offset: u32) -> usize {
    let at = (offset as usize).min(source.len());
    source.as_bytes()[..at]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// The module `rule` of a script: the view, read-only.
fn rule_module(input: &Arc<Input>) -> Module {
    let mut m = Module::new("rule");
    let view = Arc::clone(input);
    m.fn_named("get", ["name"], move |name: &str| view.get(name));
    m.fn_named("quote", ["text"], |text: &str| rules_tools::quote(text));
    m
}

/// Compile and run the script of `input` in this process: no rlimits, no
/// wall clock. [`child_main`] calls it in the child; tests call it.
pub fn execute(input: Input) -> Reply {
    if input.source.len() > rules_file::MAX_SCRIPT_BYTES {
        return Reply::failed(format!(
            "the script has {} bytes; a script has at most {}",
            input.source.len(),
            rules_file::MAX_SCRIPT_BYTES
        ));
    }
    let input = Arc::new(input);
    let ctx = Context::new().module(rule_module(&input));
    let unit = match ctx.compile(&input.source) {
        Ok(u) => u,
        Err(wscript::Error::Compile(diags)) => {
            let errors: Vec<String> = diags
                .iter()
                .filter(|d| d.severity == wscript::Severity::Error)
                .take(5)
                .map(|d| {
                    format!(
                        "line {}: [{}] {}",
                        line_of(&input.source, d.span.lo),
                        d.code,
                        d.message
                    )
                })
                .collect();
            return Reply::failed(format!(
                "the script does not compile: {}",
                errors.join("; ")
            ));
        }
        Err(e) => return Reply::failed(format!("the script does not compile: {e}")),
    };
    let config = VmConfig {
        fuel: Some(input.fuel),
        call_depth: CALL_DEPTH,
        out: Box::new(|_, _| {}),
        ..VmConfig::default()
    };
    let mut vm = Vm::with_config(&ctx, config);
    vm.set_fuel(Some(input.fuel));
    let result: Result<String, wscript::Error> = vm.call_unit(&unit, "main", ());
    let fuel_used = input.fuel.saturating_sub(vm.fuel().unwrap_or(0));
    match result {
        Ok(output) if output.len() > rules_file::MAX_SCRIPT_OUTPUT => Reply {
            output: None,
            error: Some(format!(
                "the script returned {} bytes; a script returns at most {}",
                output.len(),
                rules_file::MAX_SCRIPT_OUTPUT
            )),
            fuel_used,
        },
        Ok(output) => Reply {
            output: Some(output),
            error: None,
            fuel_used,
        },
        Err(wscript::Error::Signature(m)) => Reply {
            output: None,
            error: Some(format!("the script needs `fn main() -> string`: {m}")),
            fuel_used,
        },
        Err(e) => Reply {
            output: None,
            error: Some(format!("the script failed: {e}")),
            fuel_used,
        },
    }
}

/// Set the limits of this process (the child).
#[cfg(unix)]
fn limit_self() -> Result<(), String> {
    use nix::sys::resource::{Resource, setrlimit};
    for (r, n) in [
        (Resource::RLIMIT_DATA, MEMORY_BYTES),
        (Resource::RLIMIT_CPU, CPU_SECONDS),
        (Resource::RLIMIT_FSIZE, 0),
        (Resource::RLIMIT_CORE, 0),
        (Resource::RLIMIT_NOFILE, MAX_FILES),
    ] {
        setrlimit(r, n, n).map_err(|e| format!("cannot set {r:?}: {e}"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn limit_self() -> Result<(), String> {
    Err("scripts run on Unix only".into())
}

/// Confine this process (the child) after its rlimits (TER-545): set
/// `no_new_privs`, then a seccomp filter that allows only the calls that
/// the rest of [`child_main`] needs:
///
/// - `read` on fd 0 and `write` on fds 1 and 2 (the input, the reply,
///   a panic message),
/// - `exit` and `exit_group`,
/// - memory: `brk`, `mmap` and `mprotect` without `PROT_EXEC`, `munmap`,
///   `mremap`, `madvise`,
/// - `futex`, `rt_sigreturn`, `sigaltstack`, `clock_gettime`, and
///   `getrandom` (the keys of a std `HashMap`),
/// - an end by a signal: `abort` (a failed allocation at the memory
///   limit) sends `SIGABRT` to itself. `tgkill` only to this process,
///   `getpid`, `gettid`, and `rt_sigaction`. Without them `abort` falls
///   back to a fault, and the stack overflow handler of std, which then
///   cannot reset its action, runs that fault again without end,
/// - threads: the wscript compiler runs on a thread of its own (a 32 MiB
///   stack). `clone` only with `CLONE_THREAD` (a thread shares the
///   process, so it is no new process), `set_robust_list`, `rseq`,
///   `rt_sigprocmask` and `sched_yield`. The flags of `clone3` are in
///   memory, where seccomp cannot read them: a first filter makes
///   `clone3` fail with `ENOSYS`, and the C library then uses `clone`.
///
/// Each other call (an `open`, a `socket`, an `execve`, a `kill` of
/// another process, a `fork`) fails with `EPERM`. A call of another architecture ends the
/// process (the filter's architecture check). The filters hold for the
/// rest of the process, its threads too, and cannot be removed.
#[cfg(target_os = "linux")]
pub fn confine() -> Result<(), String> {
    use nix::libc;
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    nix::sys::prctl::set_no_new_privs().map_err(|e| format!("cannot set no_new_privs: {e}"))?;
    let bad = |e: seccompiler::BackendError| format!("cannot build the seccomp filter: {e}");
    let arch: seccompiler::TargetArch = std::env::consts::ARCH.try_into().map_err(bad)?;
    let apply = |rules: BTreeMap<i64, Vec<SeccompRule>>, mismatch, matched| {
        let filter = SeccompFilter::new(rules, mismatch, matched, arch).map_err(bad)?;
        let program: BpfProgram = filter.try_into().map_err(bad)?;
        seccompiler::apply_filter(&program)
            .map_err(|e| format!("cannot set the seccomp filter: {e}"))
    };
    // 1. `clone3` fails with ENOSYS, each other call goes on to filter 2
    //    (which allows `clone3`, so this filter's result holds).
    apply(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
    )?;
    // 2. The allow list. One rule per value: the rules of a call are or-ed.
    let arg_is = |arg: u8, op: SeccompCmpOp, value: u64| {
        SeccompCondition::new(arg, SeccompCmpArgLen::Dword, op, value)
            .and_then(|c| SeccompRule::new(vec![c]))
            .map_err(bad)
    };
    let fd = |n: u64| arg_is(0, SeccompCmpOp::Eq, n);
    let no_exec = || arg_is(2, SeccompCmpOp::MaskedEq(libc::PROT_EXEC as u64), 0);
    let thread = libc::CLONE_THREAD as u64;
    let me = u64::from(std::process::id());
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::from([
        (libc::SYS_read, vec![fd(0)?]),
        (libc::SYS_write, vec![fd(1)?, fd(2)?]),
        (libc::SYS_mmap, vec![no_exec()?]),
        (libc::SYS_mprotect, vec![no_exec()?]),
        (
            libc::SYS_clone,
            vec![arg_is(0, SeccompCmpOp::MaskedEq(thread), thread)?],
        ),
        (libc::SYS_tgkill, vec![arg_is(0, SeccompCmpOp::Eq, me)?]),
    ]);
    for call in [
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_brk,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigprocmask,
        libc::SYS_sigaltstack,
        libc::SYS_clock_gettime,
        libc::SYS_getrandom,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_sched_yield,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_rt_sigaction,
        // Filter 1 refuses it (ENOSYS). With EPERM here, this newer filter
        // would win: of two `Errno` results the kernel keeps the newest.
        libc::SYS_clone3,
    ] {
        rules.insert(call, Vec::new());
    }
    apply(
        rules,
        SeccompAction::Errno(libc::EPERM as u32),
        SeccompAction::Allow,
    )
}

/// Without seccomp a script does not run: the child fails closed.
#[cfg(not(target_os = "linux"))]
pub fn confine() -> Result<(), String> {
    Err("scripts run on Linux only (a seccomp filter)".into())
}

/// Close each file of this process (the child) from 3 up: the daemon's
/// files have close-on-exec, and this makes sure (TER-173 review). The
/// open files are read from `/proc/self/fd` (or `/dev/fd`); without
/// either, 3 to 1023 are closed.
#[cfg(unix)]
fn close_inherited() {
    let listed: Option<Vec<i32>> = ["/proc/self/fd", "/dev/fd"].iter().find_map(|d| {
        let dir = std::fs::read_dir(d).ok()?;
        Some(
            dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
                .collect(),
        )
    });
    let fds = listed.unwrap_or_else(|| (3..1024).collect());
    for fd in fds.into_iter().filter(|fd| *fd > 2) {
        // The fd of the listing itself is already closed: EBADF.
        let _ = nix::unistd::close(fd);
    }
}

#[cfg(not(unix))]
fn close_inherited() {}

/// The child: close the inherited files, set the limits and the seccomp
/// filter ([`confine`]), read the input
/// from stdin, run the script, and write the [`Reply`] to stdout. The
/// exit code is 0 when it wrote a reply.
pub fn child_main() -> i32 {
    close_inherited();
    let reply = match limit_self().and_then(|()| confine()) {
        Err(e) => Reply::failed(e),
        Ok(()) => {
            let mut buf = Vec::new();
            let read = std::io::stdin()
                .lock()
                .take(MAX_INPUT as u64 + 1)
                .read_to_end(&mut buf);
            match read {
                Err(e) => Reply::failed(format!("cannot read the input: {e}")),
                Ok(n) if n > MAX_INPUT => Reply::failed("the input is too large"),
                Ok(_) => match serde_json::from_slice::<Input>(&buf) {
                    Ok(input) => execute(input),
                    Err(e) => Reply::failed(format!("the input does not read: {e}")),
                },
            }
        }
    };
    let Ok(text) = serde_json::to_vec(&reply) else {
        return 1;
    };
    let mut out = std::io::stdout().lock();
    if out.write_all(&text).and_then(|()| out.flush()).is_err() {
        return 1;
    }
    0
}

static RUNNER: OnceLock<PathBuf> = OnceLock::new();

/// Use `path` as the binary of the children (tests: the daemon binary,
/// since the test binary is not one). Only the first call counts.
pub fn set_runner(path: PathBuf) {
    let _ = RUNNER.set(path);
}

/// The binary of the children: [`set_runner`], else this binary.
fn runner() -> Result<PathBuf, String> {
    if let Some(p) = RUNNER.get() {
        return Ok(p.clone());
    }
    std::env::current_exe().map_err(|e| format!("cannot find the daemon binary: {e}"))
}

/// The slots of a pool: `all` for every project, and at most `share` of
/// them for one project. A run first takes a slot of its project's
/// share, then one of `all`: the waits beyond a project's share queue on
/// that project alone, and the queue of `all` (first come, first served)
/// holds at most `share` waits of each project.
pub struct Slots {
    all: Arc<Semaphore>,
    share: usize,
    projects: Mutex<HashMap<String, Arc<Semaphore>>>,
}

/// A slot of a [`Slots`]: the run holds it until it ends.
pub struct Slot {
    _all: OwnedSemaphorePermit,
    _share: OwnedSemaphorePermit,
}

impl Slots {
    /// `total` slots, at most `share` of them for one project.
    pub fn new(total: usize, share: usize) -> Slots {
        Slots {
            all: Arc::new(Semaphore::new(total)),
            share: share.clamp(1, total.max(1)),
            projects: Mutex::new(HashMap::new()),
        }
    }

    /// The share of `project`. A share that nobody holds or waits for
    /// (only the map has it) has all its slots back: drop it, so the map
    /// holds only the projects that run scripts now.
    fn share_of(&self, project: &str) -> Arc<Semaphore> {
        let mut map = self
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.retain(|_, s| Arc::strong_count(s) > 1);
        map.entry(project.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(self.share)))
            .clone()
    }

    /// A slot for `project` within `deadline` (both waits), else the
    /// reason.
    pub async fn acquire(&self, project: &str, deadline: Duration) -> Result<Slot, String> {
        let share = self.share_of(project);
        let all = self.all.clone();
        let both = async move {
            let share = share.acquire_owned().await?;
            let all = all.acquire_owned().await?;
            Ok::<_, tokio::sync::AcquireError>(Slot {
                _all: all,
                _share: share,
            })
        };
        match tokio::time::timeout(deadline, both).await {
            Ok(Ok(slot)) => Ok(slot),
            Ok(Err(_)) => Err("the script slots are closed".to_owned()),
            Err(_) => Err(busy(deadline)),
        }
    }
}

/// The reason of a wait for a slot that ran out after `deadline`.
fn busy(deadline: Duration) -> String {
    format!(
        "the script slots are busy: no slot came free in {} ms; try again later",
        deadline.as_millis()
    )
}

/// The slots of a pool.
fn slots(pool: Pool) -> &'static Slots {
    static ENGINE: OnceLock<Slots> = OnceLock::new();
    static DRY: OnceLock<Slots> = OnceLock::new();
    match pool {
        Pool::Engine => ENGINE.get_or_init(|| Slots::new(MAX_RUNNING, PROJECT_ENGINE_SLOTS)),
        Pool::DryRun => DRY.get_or_init(|| Slots::new(DRY_RUN_SLOTS, PROJECT_DRY_RUN_SLOTS)),
    }
}

/// The slots of `pool` that runs hold now. For the tests, which wait
/// until a run holds its slot rather than for a fixed time (TER-680).
#[doc(hidden)]
pub fn slots_in_use(pool: Pool) -> usize {
    let total = match pool {
        Pool::Engine => MAX_RUNNING,
        Pool::DryRun => DRY_RUN_SLOTS,
    };
    total.saturating_sub(slots(pool).all.available_permits())
}

/// The holds of [`hold_slots`]: a closed gate opens the hold.
type Holds = Mutex<HashMap<(Pool, String), Arc<Semaphore>>>;

fn holds() -> &'static Holds {
    static HOLDS: OnceLock<Holds> = OnceLock::new();
    HOLDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A hold of [`hold_slots`]: dropping it lets the held runs end.
#[doc(hidden)]
pub struct SlotHold {
    key: (Pool, String),
    gate: Arc<Semaphore>,
}

impl Drop for SlotHold {
    fn drop(&mut self) {
        let mut map = holds()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.get(&self.key).is_some_and(|g| Arc::ptr_eq(g, &self.gate)) {
            map.remove(&self.key);
        }
        self.gate.close();
    }
}

/// For the tests: each run of `project` in `pool` that ends while the
/// hold lives keeps its slot until the hold is dropped, so a test knows
/// that the slot is busy without a time budget (TER-1221).
#[doc(hidden)]
pub fn hold_slots(pool: Pool, project: &str) -> SlotHold {
    let key = (pool, project.to_owned());
    let gate = Arc::new(Semaphore::new(0));
    holds()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.clone(), gate.clone());
    SlotHold { key, gate }
}

/// Wait while a [`hold_slots`] holds the runs of `project` in `pool`.
async fn held(pool: Pool, project: &str) {
    let gate = holds()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(pool, project.to_owned()))
        .cloned();
    if let Some(gate) = gate {
        // The gate has no permits: this returns when the hold closes it.
        let _ = gate.acquire().await;
    }
}

/// The pool of a script's child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pool {
    /// The runs of the engine: [`MAX_RUNNING`] slots.
    Engine,
    /// The dry runs: [`DRY_RUN_SLOTS`] slots.
    DryRun,
}

/// The time that one script of `project` may take: the wait for a slot
/// (at most `acquire`), then the run (at most `wall`), and both together
/// at most `total` when it is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    pub pool: Pool,
    pub project: String,
    pub acquire: Duration,
    pub wall: Duration,
    pub total: Option<Duration>,
}

impl Budget {
    /// A step of a run of the engine for `project`.
    pub fn engine(project: &str) -> Budget {
        Budget {
            pool: Pool::Engine,
            project: project.to_owned(),
            acquire: ENGINE_ACQUIRE,
            wall: WALL_CLOCK,
            total: None,
        }
    }

    /// A script of a dry run of `project` that has `left` of its
    /// [`DRY_RUN_TOTAL`]: one deadline of `left` for the wait and the run.
    pub fn dry_run(project: &str, left: Duration) -> Budget {
        Budget {
            pool: Pool::DryRun,
            project: project.to_owned(),
            acquire: DRY_RUN_ACQUIRE.min(left),
            wall: WALL_CLOCK.min(left),
            total: Some(left),
        }
    }

    /// The time of the run after a wait of `waited` for the slot: `wall`,
    /// and no more than what is left of `total`.
    pub fn run_time(&self, waited: Duration) -> Duration {
        match self.total {
            Some(t) => self.wall.min(t.saturating_sub(waited)),
            None => self.wall,
        }
    }
}

/// Run the script of `input` in a child process with the limits of the
/// module docs, within `budget`. `Ok` is the reply of the child (which
/// may hold an error); `Err` is a failure of the child itself (killed,
/// crashed, the wall clock) or no free slot. Call it outside any
/// database closure.
pub async fn run(input: &Input, budget: Budget) -> Result<Reply, String> {
    let body = serde_json::to_vec(input).map_err(|e| format!("cannot write the input: {e}"))?;
    if body.len() > MAX_INPUT {
        return Err(format!(
            "the script and its view have {} bytes; at most {MAX_INPUT}",
            body.len()
        ));
    }
    let asked = Instant::now();
    let _slot = slots(budget.pool)
        .acquire(&budget.project, budget.acquire)
        .await?;
    let wall = budget.run_time(asked.elapsed());
    if wall.is_zero() {
        return Err(busy(budget.acquire));
    }
    let result = run_child(body, wall).await;
    held(budget.pool, &budget.project).await;
    result
}

/// Run a child on `body` for at most `wall`: the part of [`run`] that
/// holds the slot.
async fn run_child(body: Vec<u8>, wall: Duration) -> Result<Reply, String> {
    let mut child = tokio::process::Command::new(runner()?)
        .arg(CHILD_ARG)
        .env_clear()
        .current_dir("/")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot start the script process: {e}"))?;
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let talk = async move {
        if let Some(mut w) = stdin.take() {
            // A child that ends early closes the pipe: its reply says why.
            let _ = w.write_all(&body).await;
            drop(w);
        }
        let mut buf = Vec::new();
        if let Some(r) = stdout {
            r.take(MAX_REPLY as u64 + 1)
                .read_to_end(&mut buf)
                .await
                .map_err(|e| format!("cannot read the script result: {e}"))?;
        }
        let status = child
            .wait()
            .await
            .map_err(|e| format!("cannot wait for the script process: {e}"))?;
        Ok::<_, String>((buf, status))
    };
    let (buf, status) = match tokio::time::timeout(wall, talk).await {
        Ok(r) => r?,
        // Dropping `talk` drops the child: kill_on_drop kills it.
        Err(_) => {
            return Err(format!(
                "the script ran longer than {} ms and was stopped",
                wall.as_millis()
            ));
        }
    };
    if buf.len() > MAX_REPLY {
        return Err("the script result is too large".into());
    }
    match serde_json::from_slice::<Reply>(&buf) {
        Ok(r) if status.success() => Ok(r),
        _ => Err(ended(status)),
    }
}

/// Why a child ended without a reply.
fn ended(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!(
                "the script process was ended by signal {sig} (a memory limit of {} MiB, a CPU limit of {CPU_SECONDS} s, or a crash)",
                MEMORY_BYTES / (1024 * 1024)
            );
        }
    }
    format!("the script process ended without a result ({status})")
}

/// The verbs of a script step: run the script of `input`, then read its
/// output with [`rules_file::script_actions`]. `Ok` holds the verbs and
/// the fuel used; `Err` is the reason for the run's failure.
pub async fn actions(
    input: &Input,
    ctx: &rules_file::Context,
    budget: Budget,
) -> Result<(Vec<rules_file::Action>, u64), String> {
    let reply = run(input, budget).await?;
    if let Some(e) = reply.error {
        return Err(e);
    }
    let out = reply.output.unwrap_or_default();
    let actions = rules_file::script_actions(&out, ctx)?;
    Ok((actions, reply.fuel_used))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(source: &str) -> Input {
        Input {
            source: source.into(),
            fuel: 10_000,
            values: BTreeMap::from([
                ("card.column".to_owned(), "review".to_owned()),
                ("card.labels".to_owned(), "feature, type:hard".to_owned()),
            ]),
            event: Some(EventInput {
                seq: 7,
                kind: "card".into(),
                summary: "moved".into(),
                project: Some("p".into()),
                actor: Some("p/a".into()),
                data: json!({"to": "review", "n": 3}),
            }),
        }
    }

    #[test]
    fn a_script_reads_the_view_and_returns_verbs() {
        let r = execute(input(
            r#"use rule
fn main() -> string {
  let col = rule::get("card.column").unwrap_or("")
  let to = rule::get("event.data.to").unwrap_or("")
  let kind = rule::get("event.kind").unwrap_or("")
  if col == "review" && to == "review" && kind == "card" {
    return "card_comment {{ text = " + rule::quote("in \"review\"") + " }}"
  }
  ""
}"#,
        ));
        assert_eq!(r.error, None, "{r:?}");
        let out = r.output.unwrap_or_default();
        assert_eq!(out, r#"card_comment { text = "in \"review\"" }"#);
        assert!(r.fuel_used > 0);
        let a = rules_file::script_actions(&out, &rules_file::Context::default());
        let a = a.unwrap_or_default();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].verb, "card.comment");
        assert_eq!(a[0].args["text"], json!("in \"review\""));
    }

    #[test]
    fn a_missing_value_is_none() {
        let r = execute(input(
            r#"use rule
fn main() -> string {
  if rule::get("event.data.nope").is_none() && rule::get("job.id").is_none() { "none" } else { "some" }
}"#,
        ));
        assert_eq!(r.output.as_deref(), Some("none"), "{r:?}");
    }

    #[test]
    fn a_loop_runs_out_of_fuel() {
        let r = execute(input("fn main() -> string { loop { } \n \"\" }"));
        let e = r.error.unwrap_or_default();
        assert!(e.contains("fuel"), "{e}");
    }

    #[test]
    fn the_stdlib_modules_are_not_there() {
        for m in ["fs", "process", "json", "env", "math"] {
            let r = execute(input(&format!("use {m}\nfn main() -> string {{ \"\" }}")));
            let e = r.error.unwrap_or_default();
            assert!(e.contains("does not compile"), "{m}: {e}");
        }
    }

    #[test]
    fn compile_errors_name_the_line() {
        let r = execute(input("fn main() -> string {\n  undefined_fn()\n}"));
        let e = r.error.unwrap_or_default();
        assert!(e.contains("line 2"), "{e}");
    }

    #[test]
    fn a_script_without_main_fails() {
        let r = execute(input("fn other() -> string { \"\" }"));
        let e = r.error.unwrap_or_default();
        assert!(e.contains("fn main() -> string"), "{e}");
    }

    #[test]
    fn deep_recursion_stops_at_the_call_depth() {
        let r = execute(Input {
            fuel: 1_000_000,
            ..input("fn f(n: int) -> int { f(n + 1) }\nfn main() -> string { f(0)\n \"\" }")
        });
        assert!(r.error.is_some(), "{r:?}");
    }

    #[test]
    fn a_large_output_fails() {
        let r = execute(Input {
            fuel: 1_000_000,
            ..input(r#"fn main() -> string { "x".repeat(20000) }"#)
        });
        let e = r.error.unwrap_or_default();
        assert!(e.contains("at most"), "{e}");
    }

    #[test]
    fn print_writes_nowhere() {
        let r = execute(input(
            r#"fn main() -> string { println("[User] hello")
 "" }"#,
        ));
        assert_eq!(r.output.as_deref(), Some(""), "{r:?}");
    }

    #[tokio::test]
    async fn a_busy_pool_refuses_after_its_deadline() {
        let busy = Slots::new(1, 1);
        let _held = busy.acquire("other", Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let e = busy
            .acquire("p", Duration::from_millis(50))
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("slots are busy"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(2));
        let free = Slots::new(1, 1);
        assert!(free.acquire("p", Duration::from_millis(50)).await.is_ok());
    }

    #[tokio::test]
    async fn one_project_holds_at_most_its_share() {
        let pool = Slots::new(MAX_RUNNING, PROJECT_ENGINE_SLOTS);
        let short = Duration::from_millis(50);
        let mut held = Vec::new();
        for _ in 0..PROJECT_ENGINE_SLOTS {
            held.push(pool.acquire("a", short).await.ok());
        }
        assert!(held.iter().all(Option::is_some));
        // A third slot for "a" waits for its share, though slots are free.
        let e = pool.acquire("a", short).await.err().unwrap_or_default();
        assert!(e.contains("slots are busy"), "{e}");
        // The other projects still get the rest of the pool.
        let b = pool.acquire("b", short).await;
        let c = pool.acquire("c", short).await;
        assert!(b.is_ok() && c.is_ok());
        // The pool is full now: a fourth project waits.
        assert!(pool.acquire("d", short).await.is_err());
        // A slot that "a" gives back is again for "a".
        held.pop();
        assert!(pool.acquire("a", short).await.is_ok());
    }

    #[tokio::test]
    async fn the_waits_of_one_project_do_not_queue_before_another() {
        // One dry-run slot, held by "a"; "a" has more dry runs waiting.
        let pool = Arc::new(Slots::new(DRY_RUN_SLOTS, PROJECT_DRY_RUN_SLOTS));
        let held = pool.acquire("a", Duration::from_millis(50)).await.ok();
        assert!(held.is_some());
        let mut waits = Vec::new();
        for _ in 0..3 {
            let pool = pool.clone();
            waits.push(tokio::spawn(async move {
                pool.acquire("a", Duration::from_secs(5)).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let b = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.acquire("b", Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        // "b" queued after the 3 waits of "a", and gets the next slot.
        drop(held);
        let b = tokio::time::timeout(Duration::from_secs(1), b).await;
        assert!(matches!(b, Ok(Ok(Ok(_)))), "b waited behind a");
        for w in waits {
            w.abort();
        }
    }

    #[test]
    fn the_budgets_stay_in_their_limits() {
        let e = Budget::engine("p");
        assert_eq!(e.pool, Pool::Engine);
        assert_eq!(e.project, "p");
        assert_eq!(e.acquire, ENGINE_ACQUIRE);
        assert_eq!(e.run_time(Duration::from_secs(20)), WALL_CLOCK);
        let d = Budget::dry_run("p", Duration::from_secs(1));
        assert_eq!(d.pool, Pool::DryRun);
        assert_eq!(d.wall, Duration::from_secs(1));
        assert_eq!(d.acquire, Duration::from_secs(1));
        let d = Budget::dry_run("p", DRY_RUN_TOTAL);
        assert_eq!((d.acquire, d.wall), (DRY_RUN_ACQUIRE, WALL_CLOCK));
    }

    #[test]
    fn a_dry_run_has_one_deadline_for_the_wait_and_the_run() {
        // 3 s left: a wait of 2 s leaves 1 s to run, not the 3 s of wall.
        let d = Budget::dry_run("p", Duration::from_secs(3));
        assert_eq!(d.run_time(Duration::ZERO), Duration::from_secs(3));
        assert_eq!(d.run_time(DRY_RUN_ACQUIRE), Duration::from_secs(1));
        assert_eq!(d.run_time(Duration::from_secs(4)), Duration::ZERO);
        // The wait and the run of the last script end by DRY_RUN_TOTAL.
        let left = Duration::from_secs(4);
        let d = Budget::dry_run("p", left);
        assert!(d.acquire + d.run_time(d.acquire) <= left);
    }

    /// The env of [`a_confined_child_runs_a_script_and_cannot_open_a_file`]
    /// in its copy of this test binary: confine, then probe.
    #[cfg(target_os = "linux")]
    const PROBE_ENV: &str = "TERRARIUM_TEST_CONFINE_PROBE";

    /// The probe (TER-545): in a copy of this test binary, [`confine`]
    /// this thread like a script child, then read stdin, run a script,
    /// try an open and an exec, and write what happened to stdout.
    /// Without [`PROBE_ENV`] it does nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn confine_probe() {
        if std::env::var_os(PROBE_ENV).is_none() {
            return;
        }
        let mut out = std::io::stdout();
        if let Err(e) = confine() {
            let _ = writeln!(out, "confine: {e}");
            std::process::exit(2);
        }
        let mut stdin = String::new();
        let read = std::io::stdin().read_to_string(&mut stdin).map(|_| stdin);
        let script = execute(input("fn main() -> string { \"from the script\" }"));
        let open = std::fs::File::open("/etc/hostname").map(|_| ());
        let exec = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .status()
            .map(|_| ());
        // A signal to another process (init): refused, a signal to itself is not.
        let kill = nix::sys::signal::kill(nix::unistd::Pid::from_raw(1), None);
        let text = format!(
            "read: {read:?}\nscript: {:?} {:?}\nopen: {:?}\nexec: {:?}\nkill: {kill:?}\n",
            script.output,
            script.error,
            open.map_err(|e| e.raw_os_error()),
            exec.map_err(|e| e.raw_os_error()),
        );
        let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
        // End here: libtest would go on with calls that the filter refuses.
        std::process::exit(0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_confined_child_runs_a_script_and_cannot_open_a_file() {
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .args([
                "rule_script::tests::confine_probe",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE_ENV, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"the input").unwrap();
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{:?}: {text}", out.status);
        let eperm = Some(nix::libc::EPERM);
        assert!(text.contains("read: Ok(\"the input\")"), "{text}");
        assert!(
            text.contains("script: Some(\"from the script\") None"),
            "{text}"
        );
        assert!(text.contains(&format!("open: Err({eperm:?})")), "{text}");
        assert!(text.contains(&format!("exec: Err({eperm:?})")), "{text}");
        assert!(text.contains("kill: Err(EPERM)"), "{text}");
    }

    #[test]
    fn the_view_holds_the_template_names() {
        let names: Vec<String> = view_names().collect();
        for n in ["card.column", "job.state", "group.key", "rule.name"] {
            assert!(names.iter().any(|x| x == n), "{n}");
        }
    }
}
