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
//!   to result (then the child is killed), and at most [`MAX_RUNNING`]
//!   children at a time.
//! - **No database lock.** The engine takes the view in a write, then
//!   runs the script after that write, then checks and records the
//!   returned verbs in the next write (`crate::rules_engine`). The dry run
//!   runs its scripts after its read.
//!
//! The script returns verb blocks in the syntax of a rules file; the
//! engine reads them with [`rules_file::script_actions`] and runs them in
//! place of the step, as the rule actor, with the store's checks.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use terrarium_core::rules_file;
use terrarium_core::rules_tools;
use terrarium_core::template::{EventView, value_text};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
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
/// The most children at a time.
pub const MAX_RUNNING: usize = 4;
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
    source.as_bytes()[..at].iter().filter(|b| **b == b'\n').count() + 1
}

/// The module `rule` of a script: the view, read-only.
fn rule_module(input: &Arc<Input>) -> Module {
    let mut m = Module::new("rule");
    let view = Arc::clone(input);
    m.fn_named("get", &["name"], move |name: &str| view.get(name));
    m.fn_named("quote", &["text"], |text: &str| rules_tools::quote(text));
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

/// The child: set the limits, read the input from stdin, run the script,
/// and write the [`Reply`] to stdout. The exit code is 0 when it wrote a
/// reply.
pub fn child_main() -> i32 {
    let reply = match limit_self() {
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

fn running() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| Semaphore::new(MAX_RUNNING))
}

/// Run the script of `input` in a child process with the limits of the
/// module docs. `Ok` is the reply of the child (which may hold an
/// error); `Err` is a failure of the child itself (killed, crashed, the
/// wall clock). Call it outside any database closure.
pub async fn run(input: &Input) -> Result<Reply, String> {
    let body = serde_json::to_vec(input).map_err(|e| format!("cannot write the input: {e}"))?;
    if body.len() > MAX_INPUT {
        return Err(format!(
            "the script and its view have {} bytes; at most {MAX_INPUT}",
            body.len()
        ));
    }
    let _slot = running()
        .acquire()
        .await
        .map_err(|_| "the script slots are closed".to_owned())?;
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
    let (buf, status) = match tokio::time::timeout(WALL_CLOCK, talk).await {
        Ok(r) => r?,
        // Dropping `talk` drops the child: kill_on_drop kills it.
        Err(_) => {
            return Err(format!(
                "the script ran longer than {} seconds and was stopped",
                WALL_CLOCK.as_secs()
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
) -> Result<(Vec<rules_file::Action>, u64), String> {
    let reply = run(input).await?;
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
            let r = execute(input(&format!(
                "use {m}\nfn main() -> string {{ \"\" }}"
            )));
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

    #[test]
    fn the_view_holds_the_template_names() {
        let names: Vec<String> = view_names().collect();
        for n in ["card.column", "job.state", "group.key", "rule.name"] {
            assert!(names.iter().any(|x| x == n), "{n}");
        }
    }
}
