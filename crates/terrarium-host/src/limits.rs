//! Resource limits on a `host` target (docs/host-protocol.md, "Resource
//! limits"), with the systemd user manager.
//!
//! Each holder of a target has one budget: a slice of the user manager,
//! `terrarium-<target>-<holder>.slice`. Before an `exec` with limits, the
//! host agent sets the limits on that slice with `systemctl --user
//! set-property --runtime`. It then starts the process with `systemd-run
//! --user --scope --slice=<slice>`. `systemd-run` puts itself in a new scope
//! and then runs the command in its own process, so the PID and the process
//! group stay those of the session. All the scopes of one holder share the
//! slice, so all its sessions share the budget.
//!
//! A kill of the session also kills the scope (`systemctl --user kill
//! --signal=SIGKILL`), so a process that left the process group ends too.
//! A scope is in the user manager, not in the host agent's own cgroup, so
//! a stop of the host agent does not end it. On start, the host agent
//! kills the scopes that a run before it left ([`kill_leftovers`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::protocol::{Limits, LimitsCapability};
use crate::session::ExecPlan;

/// The `systemd-run` binary. It must be on `PATH`.
pub const SYSTEMD_RUN: &str = "systemd-run";
/// The `systemctl` binary. It must be on `PATH`.
pub const SYSTEMCTL: &str = "systemctl";
/// The longest unit name that systemd takes, with its suffix.
const UNIT_NAME_MAX: usize = 255;
/// How long a `systemctl` or `systemd-run` call of the host agent may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Escape a string for a unit name, as `systemd-escape` does: `/` becomes
/// `-`, and every byte other than `[A-Za-z0-9:_.]` becomes `\xNN`. A `.`
/// at the start is escaped too.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'/' => out.push('-'),
            b'.' if i == 0 => out.push_str("\\x2e"),
            b if b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.') => {
                out.push(char::from(b));
            }
            b => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// The slice of one holder on one target:
/// `terrarium-<target>-<project>-<agent>.slice`, each part escaped, so
/// systemd nests it under `terrarium-<target>.slice` and
/// `terrarium.slice`. The operator's slice is
/// `terrarium-<target>-_operator.slice`: no project name starts with `_`.
/// A name that is too long for systemd becomes `terrarium-x<hash>.slice`.
pub fn slice_name(target: &str, holder: &str) -> String {
    if let Some(target) = target.strip_prefix("_exclusive/") {
        return slice_name(target, holder).replacen("terrarium-", "terrariumreserved-", 1);
    }
    if target.is_empty() && holder.is_empty() {
        return "terrarium.slice".into();
    }
    if holder.is_empty() {
        return format!("terrarium-{}.slice", escape(target));
    }
    let holder = if holder.contains('/') {
        escape(holder)
    } else {
        format!("_{}", escape(holder))
    };
    let name = format!("terrarium-{}-{holder}.slice", escape(target));
    if name.len() <= UNIT_NAME_MAX {
        return name;
    }
    let mut h = Sha256::new();
    h.update(target.as_bytes());
    h.update([0]);
    h.update(holder.as_bytes());
    let hex = crate::protocol::hex(&h.finalize());
    format!("terrarium-x{}.slice", &hex[..32])
}

/// The start of every scope name of this host agent.
pub fn scope_prefix(host: &str) -> String {
    format!("terrarium-exec-{}-", escape(host))
}

/// The scope of one session: `terrarium-exec-<host>-<session>-<nonce>.scope`.
/// The nonce keeps the name free when a process of an older session with
/// the same ID still holds its scope.
pub fn scope_name(host: &str, session: u32, nonce: u32) -> String {
    let name = format!("{}{session}-{nonce:08x}.scope", scope_prefix(host));
    if name.len() <= UNIT_NAME_MAX {
        name
    } else {
        format!("terrarium-exec-{session}-{nonce:08x}.scope")
    }
}

/// The slice properties of `limits`. A limit that is not set gets the
/// systemd default, so a limit that the operator removed does not stay.
/// `MemoryHigh` is 90 % of `MemoryMax`: the kernel slows the holder down
/// before the OOM killer acts. With `memory` and no `swap`, the holder
/// gets no swap.
pub fn properties(limits: &Limits) -> Vec<String> {
    let mut p = Vec::new();
    p.push(match limits.cpu_millis {
        Some(m) => format!("CPUQuota={}%", m / 10),
        None => "CPUQuota=".to_owned(),
    });
    match limits.memory_bytes {
        Some(max) => {
            let high = (max / 10 * 9) & !4095;
            p.push(format!("MemoryMax={max}"));
            p.push(format!("MemoryHigh={high}"));
            p.push(format!("MemorySwapMax={}", limits.swap_bytes.unwrap_or(0)));
        }
        None => {
            p.push("MemoryMax=infinity".to_owned());
            p.push("MemoryHigh=infinity".to_owned());
            p.push("MemorySwapMax=infinity".to_owned());
        }
    }
    p.push(match limits.pids {
        Some(n) => format!("TasksMax={n}"),
        None => "TasksMax=infinity".to_owned(),
    });
    p.push(match limits.io_weight {
        Some(w) => format!("IOWeight={w}"),
        None => "IOWeight=".to_owned(),
    });
    p
}

/// The `systemctl` arguments that set the limits on the slice.
pub fn set_property_args(slice: &str, limits: &Limits) -> Vec<String> {
    let mut a = vec![
        "--user".to_owned(),
        "set-property".to_owned(),
        "--runtime".to_owned(),
        "--".to_owned(),
        slice.to_owned(),
    ];
    a.extend(properties(limits));
    a
}

/// Run the plan in a new scope in `slice`: the program becomes
/// `systemd-run`, and the old program and its arguments go after `--`.
pub fn wrap(plan: &mut ExecPlan, slice: &str, scope: &str) {
    let mut args = vec![
        "--user".to_owned(),
        "--scope".to_owned(),
        "--quiet".to_owned(),
        "--collect".to_owned(),
        format!("--slice={slice}"),
        format!("--unit={scope}"),
        "--".to_owned(),
        std::mem::replace(&mut plan.program, SYSTEMD_RUN.to_owned()),
    ];
    args.append(&mut plan.args);
    plan.args = args;
    for (k, v) in user_bus_env() {
        plan.env.entry(k).or_insert(v);
    }
    plan.scope = Some(scope.to_owned());
}

/// `systemctl --user` and `systemd-run --user` need `XDG_RUNTIME_DIR` to
/// find the user manager. A host agent in a system unit (`User=`) does not
/// have it. Then it is `/run/user/<uid>`, when that exists.
pub fn user_bus_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        let dir = format!("/run/user/{}", nix::unistd::getuid());
        if Path::new(&dir).is_dir() {
            env.insert("XDG_RUNTIME_DIR".to_owned(), dir);
        }
    }
    env
}

/// Run a `systemctl` or `systemd-run` call to its end. The error holds
/// the first line of stderr.
async fn call(bin: &str, args: &[String]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .envs(user_bus_env())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(CALL_TIMEOUT, cmd.output()).await {
        Err(_) => return Err(format!("`{bin}` did not end in {CALL_TIMEOUT:?}")),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("`{bin}` is not on PATH"));
        }
        Ok(Err(e)) => return Err(format!("cannot run `{bin}`: {e}")),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let line = err
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    Err(format!(
        "`{bin} {}` failed ({}): {line}",
        args.iter().take(3).cloned().collect::<Vec<_>>().join(" "),
        out.status
    ))
}

/// Set the limits on the slice. A failure stops the session: it never
/// runs without its limits.
pub async fn apply_slice(slice: &str, limits: &Limits) -> Result<(), String> {
    call(SYSTEMCTL, &set_property_args(slice, limits))
        .await
        .map(|_| ())
}

/// Complete slice settings for a reservation, including clearing device IO
/// entries left by an earlier grant. Only numeric device IDs enter properties.
pub fn reservation_properties(
    l: &terrarium_core::resources::RuntimeLimits,
) -> Result<Vec<String>, String> {
    l.validate()?;
    let mut p = properties(&l.limits);
    p.push(format!("AllowedCPUs={}", l.cpuset()));
    for key in [
        "IOReadBandwidthMax",
        "IOWriteBandwidthMax",
        "IOReadIOPSMax",
        "IOWriteIOPSMax",
    ] {
        let entries: Vec<String> =
            l.io.iter()
                .filter_map(|(device, io)| {
                    let value = match key {
                        "IOReadBandwidthMax" => io.read_bps,
                        "IOWriteBandwidthMax" => io.write_bps,
                        "IOReadIOPSMax" => io.read_iops,
                        _ => io.write_iops,
                    };
                    (value > 0).then(|| format!("/dev/block/{device} {value}"))
                })
                .collect();
        p.push(format!("{key}={}", entries.join(" ")));
    }
    Ok(p)
}

pub fn reservation_plan(
    target: &str,
    holder: &str,
    l: &terrarium_core::resources::RuntimeLimits,
) -> Result<crate::verb::VerbPlan, crate::verb::BuildError> {
    let mut args = vec![
        "--user".into(),
        "set-property".into(),
        "--runtime".into(),
        "--".into(),
        slice_name(target, holder),
    ];
    args.extend(reservation_properties(l).map_err(crate::verb::BuildError::bad)?);
    Ok(crate::verb::VerbPlan::plain(ExecPlan {
        program: SYSTEMCTL.into(),
        args,
        env: user_bus_env(),
        timeout: Some(CALL_TIMEOUT),
        ..Default::default()
    }))
}

pub async fn apply_reservation(
    target: &str,
    holder: &str,
    l: &terrarium_core::resources::RuntimeLimits,
) -> Result<(), String> {
    let mut args = vec![
        "--user".into(),
        "set-property".into(),
        "--runtime".into(),
        "--".into(),
        slice_name(target, holder),
    ];
    args.extend(reservation_properties(l)?);
    call(SYSTEMCTL, &args).await.map(|_| ())
}

/// Kill every process in the scope. It does not wait.
pub fn reservation_stop_plan(
    target: &str,
    holder: &str,
) -> Result<crate::verb::VerbPlan, crate::verb::BuildError> {
    Ok(crate::verb::VerbPlan::plain(ExecPlan {
        program: SYSTEMCTL.into(),
        args: vec![
            "--user".into(),
            "stop".into(),
            "--".into(),
            slice_name(target, holder),
        ],
        env: user_bus_env(),
        timeout: Some(CALL_TIMEOUT),
        ..Default::default()
    }))
}

/// `systemctl --user stop` of a holder slice, from the native shim
/// ([`crate::native::shim`]). A slice that is not loaded (exit 5) has
/// nothing to stop.
pub async fn stop_user_slice(target: &str, holder: &str) -> Result<(), String> {
    let slice = slice_name(target, holder);
    let mut cmd = tokio::process::Command::new(SYSTEMCTL);
    cmd.args(["--user", "stop", "--", &slice])
        .envs(user_bus_env())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(CALL_TIMEOUT, cmd.output()).await {
        Err(_) => return Err(format!("`{SYSTEMCTL} --user stop {slice}` did not end")),
        Ok(Err(e)) => return Err(format!("cannot run `{SYSTEMCTL}`: {e}")),
        Ok(Ok(out)) => out,
    };
    match out.status.code() {
        Some(0 | 5) => Ok(()),
        _ => Err(format!(
            "`{SYSTEMCTL} --user stop {slice}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

pub fn kill_scope(scope: &str) {
    let args = [
        "--user".to_owned(),
        "kill".to_owned(),
        "--signal=SIGKILL".to_owned(),
        "--".to_owned(),
        scope.to_owned(),
    ];
    let scope = scope.to_owned();
    tokio::spawn(async move {
        // A scope that ended already is not an error worth a warning.
        if let Err(e) = call(SYSTEMCTL, &args).await {
            tracing::debug!(%scope, error = %e, "scope kill");
        }
    });
}

/// The controllers in a `cgroup.controllers` file, sorted.
pub fn parse_controllers(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    v.sort();
    v.dedup();
    v
}

/// The unit names in `systemctl list-units --plain --no-legend` output
/// that start with `prefix`.
pub fn parse_units(text: &str, prefix: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| u.starts_with(prefix) && u.ends_with(".scope"))
        .map(str::to_owned)
        .collect()
}

/// Detect what this host can do for limits. `systemd` is true when
/// `systemd-run --user --scope -p MemoryMax=64M true` works. `controllers`
/// comes from the `cgroup.controllers` of the user manager's cgroup.
/// `docker` is the caller's part.
pub async fn detect() -> LimitsCapability {
    let mut cap = LimitsCapability::default();
    if !cfg!(target_os = "linux") {
        cap.reason = Some("resource limits need Linux with systemd".into());
        return cap;
    }
    let probe = [
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "-p",
        "MemoryMax=64M",
        "true",
    ]
    .map(str::to_owned);
    if let Err(e) = call(SYSTEMD_RUN, &probe).await {
        cap.reason = Some(format!(
            "{e}; the host agent's user needs a systemd user manager (`loginctl enable-linger \
             <user>` for a service user)"
        ));
        return cap;
    }
    let show = ["--user", "show", "--property=ControlGroup", "--value"].map(str::to_owned);
    let cgroup = match call(SYSTEMCTL, &show).await {
        Ok(cg) if cg.trim().starts_with('/') => cg.trim().to_owned(),
        Ok(cg) => {
            cap.reason = Some(format!("the user manager has no cgroup ({:?})", cg.trim()));
            return cap;
        }
        Err(e) => {
            cap.reason = Some(e);
            return cap;
        }
    };
    let file = Path::new("/sys/fs/cgroup")
        .join(cgroup.trim_start_matches('/'))
        .join("cgroup.controllers");
    match std::fs::read_to_string(&file) {
        Ok(text) => {
            cap.controllers = parse_controllers(&text);
            cap.systemd = true;
        }
        Err(e) => {
            cap.reason = Some(format!(
                "cannot read {} ({e}); resource limits need cgroup v2",
                file.display()
            ));
        }
    }
    cap
}

/// Kill the scopes of this host agent that an earlier run left: their
/// sessions ended with that run.
pub async fn kill_leftovers(host: &str) {
    let list = [
        "--user",
        "list-units",
        "--type=scope",
        "--all",
        "--plain",
        "--no-legend",
        "--no-pager",
        "terrarium-exec-*",
    ]
    .map(str::to_owned);
    let units = match call(SYSTEMCTL, &list).await {
        Ok(text) => parse_units(&text, &scope_prefix(host)),
        Err(e) => {
            tracing::debug!(error = %e, "cannot list leftover scopes");
            return;
        }
    };
    for u in units {
        tracing::warn!(scope = %u, "killing a scope that an earlier run left");
        kill_scope(&u);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_follows_systemd_escape() {
        // `systemd-escape 'terra/w-ter-3'` prints `terra-w\x2dter\x2d3`.
        assert_eq!(escape("terra/w-ter-3"), "terra-w\\x2dter\\x2d3");
        assert_eq!(escape("box_1.a:b"), "box_1.a:b");
        assert_eq!(escape(".hidden"), "\\x2ehidden");
        assert_eq!(escape("a b"), "a\\x20b");
        assert_eq!(escape("é"), "\\xc3\\xa9");
    }

    #[test]
    fn slices_nest_by_target_and_never_collide() {
        assert_eq!(
            slice_name("terra-box", "terra/w-1"),
            "terrarium-terra\\x2dbox-terra-w\\x2d1.slice"
        );
        assert_eq!(
            slice_name("box", "operator"),
            "terrarium-box-_operator.slice"
        );
        // A dash in a name is escaped, so these two do not meet.
        assert_ne!(slice_name("a-b", "p/c"), slice_name("a", "b-p/c"));
        assert_ne!(slice_name("a", "p/b-c"), slice_name("a", "p-b/c"));
        // Never the parent slice of a project named `operator`.
        assert!(
            !slice_name("t", "operator/x")
                .starts_with(slice_name("t", "operator").trim_end_matches(".slice"))
        );
        let long = slice_name(
            &"t-".repeat(31),
            &format!("{}/{}", "p-".repeat(31), "a-".repeat(31)),
        );
        assert!(
            long.len() <= UNIT_NAME_MAX && long.starts_with("terrarium-x"),
            "{long}"
        );
    }

    #[test]
    fn scope_names_carry_the_host_and_a_nonce() {
        assert_eq!(
            scope_name("lab-1", 7, 0xab),
            "terrarium-exec-lab\\x2d1-7-000000ab.scope"
        );
        let units = "terrarium-exec-lab\\x2d1-7-000000ab.scope loaded active running x\n\
                     terrarium-exec-lab-3-00000001.scope loaded active running y\n\
                     terrarium-exec-lab\\x2d1-9-00000002.scope loaded inactive dead z\n";
        assert_eq!(
            parse_units(units, &scope_prefix("lab-1")),
            [
                "terrarium-exec-lab\\x2d1-7-000000ab.scope",
                "terrarium-exec-lab\\x2d1-9-00000002.scope"
            ]
        );
        assert_eq!(
            parse_units(units, &scope_prefix("lab")),
            ["terrarium-exec-lab-3-00000001.scope"]
        );
    }

    #[test]
    fn properties_map_each_limit() {
        let l = Limits {
            cpu_millis: Some(2000),
            memory_bytes: Some(4 << 30),
            swap_bytes: None,
            pids: Some(512),
            io_weight: Some(200),
        };
        assert_eq!(
            set_property_args("s.slice", &l),
            [
                "--user",
                "set-property",
                "--runtime",
                "--",
                "s.slice",
                "CPUQuota=200%",
                "MemoryMax=4294967296",
                "MemoryHigh=3865468928",
                "MemorySwapMax=0",
                "TasksMax=512",
                "IOWeight=200"
            ]
        );
        let half = Limits {
            cpu_millis: Some(500),
            memory_bytes: Some(1 << 30),
            swap_bytes: Some(512 << 20),
            ..Default::default()
        };
        let p = properties(&half);
        assert!(p.contains(&"CPUQuota=50%".to_owned()));
        assert!(p.contains(&"MemorySwapMax=536870912".to_owned()));
        // 90 % of 1 GiB, down to a whole page.
        assert!(p.contains(&"MemoryHigh=966365184".to_owned()));
        // A limit that is not set goes back to the default.
        assert_eq!(
            properties(&Limits::default()),
            [
                "CPUQuota=",
                "MemoryMax=infinity",
                "MemoryHigh=infinity",
                "MemorySwapMax=infinity",
                "TasksMax=infinity",
                "IOWeight="
            ]
        );
    }

    #[test]
    fn wrap_keeps_the_command_after_the_dashes() {
        let mut plan = ExecPlan {
            program: "sh".into(),
            args: vec!["-c".into(), "--slice=x; echo hi".into()],
            ..Default::default()
        };
        wrap(
            &mut plan,
            "terrarium-b-p-a.slice",
            "terrarium-exec-h-1-00000001.scope",
        );
        assert_eq!(plan.program, "systemd-run");
        assert_eq!(
            plan.args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=terrarium-b-p-a.slice",
                "--unit=terrarium-exec-h-1-00000001.scope",
                "--",
                "sh",
                "-c",
                "--slice=x; echo hi"
            ]
        );
        assert_eq!(
            plan.scope.as_deref(),
            Some("terrarium-exec-h-1-00000001.scope")
        );
    }

    #[test]
    fn controllers_parse_sorted() {
        assert_eq!(
            parse_controllers("cpuset cpu io memory pids\n"),
            ["cpu", "cpuset", "io", "memory", "pids"]
        );
        assert!(parse_controllers("\n").is_empty());
    }
}
