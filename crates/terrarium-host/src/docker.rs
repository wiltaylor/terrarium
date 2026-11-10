//! `docker.*` verbs: command builders and output parsers.
//!
//! Each verb becomes one `docker` CLI process, run as an exec session.
//! Environment values never go on the command line or into the CLI's own
//! environment: the builder writes them to a 0600 `--env-file`, removed when
//! the CLI exits. So the CLI keeps the host agent's own `DOCKER_*` env
//! (`DOCKER_HOST`, `DOCKER_CONTEXT`, `DOCKER_CONFIG`, ...), and a command
//! env cannot change which daemon the CLI talks to, or how.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::config::PathPolicy;
use crate::protocol::{
    DockerExec, DockerInspect, DockerPs, DockerRm, DockerRun, DockerStart, DockerStop,
    DockerVolumeCreate, DockerVolumeRm, Limits, TargetState, ToolCapability,
};
use crate::session::{ExecPlan, PrivateFile};
use crate::verb::{BuildError, ResultParser, VerbPlan, env_key, ident, secs};

fn base(bin: &str, args: Vec<String>, timeout: Option<u64>) -> ExecPlan {
    ExecPlan {
        program: bin.to_owned(),
        args,
        timeout: secs(timeout),
        ..Default::default()
    }
}

fn s(v: &str) -> String {
    v.to_owned()
}

/// The longest `KEY=VALUE` line that goes into the env file. The CLI reads
/// the file line by line with a 64 KiB limit.
pub const ENV_FILE_LINE_MAX: usize = 60 * 1024;

/// The name prefix of the env files in [`crate::client::ClientOptions::env_dir`].
pub const ENV_FILE_PREFIX: &str = "terrarium-env-";

/// Names that the host agent refuses for a multi-line value, on top of the
/// daemon's grant deny list ([`terrarium_core::env_deny`]): the `docker`
/// CLI's own names and the C library names that the grant list does not
/// have. A multi-line value goes into the CLI's own environment (`-e KEY`),
/// which the CLI's children (credential helpers, the ssh connection helper)
/// inherit too.
pub const CLI_DENIED_ENV: &[&str] = &["GODEBUG", "GETCONF_DIR", "NIS_PATH", "RESOLV_HOST_CONF"];

/// Name prefixes of [`CLI_DENIED_ENV`]. Each name that ends in `_PROXY` in
/// any case is refused too.
pub const CLI_DENIED_ENV_PREFIXES: &[&str] = &["DOCKER_", "SSL_", "OTEL_"];

/// The exact names on the grant deny list that the fallback lets through:
/// the daemon sends multi-line values under them. `TERRARIUM_GIT_TRAILERS`
/// goes on each agent exec (terrariumd `executor.rs` `start_exec_secrets`), and
/// `TERRARIUM_COMMIT_HOOK`, `TERRARIUM_GIT_KNOWN_HOSTS` and
/// `TERRARIUM_GIT_SSH_KEY` go on a checkout (`checkout_env`,
/// `checkout_on_target`). Neither the `docker` CLI nor its children read
/// them. A grant cannot set them, but the daemon does not check the names
/// in an agent's own exec env (`ExecBody.env`, terrariumd
/// `api/targets.rs`), so the list is exact: any other `TERRARIUM_*` name
/// with a multi-line value is refused (TER-541).
pub const CLI_ALLOWED_ENV: &[&str] = &[
    "TERRARIUM_GIT_TRAILERS",
    "TERRARIUM_GIT_KNOWN_HOSTS",
    "TERRARIUM_COMMIT_HOOK",
    "TERRARIUM_GIT_SSH_KEY",
];

/// Does `k` steer the `docker` CLI, its children, or the loader under it?
/// The grant deny list except [`CLI_ALLOWED_ENV`], plus
/// [`CLI_DENIED_ENV`] and [`CLI_DENIED_ENV_PREFIXES`].
pub fn steers_the_cli(k: &str) -> bool {
    (terrarium_core::env_deny::denied(k) && !CLI_ALLOWED_ENV.contains(&k))
        || CLI_DENIED_ENV.contains(&k)
        || CLI_DENIED_ENV_PREFIXES.iter().any(|p| k.starts_with(p))
        || k.to_ascii_uppercase().ends_with("_PROXY")
}

/// Add the command env to `args`. Each single-line value goes into a 0600
/// env file in `env_dir` (`--env-file`). A multi-line value (a newline or a
/// carriage return, for example a key) cannot go on one line: it falls back
/// to `-e KEY` with the value in the CLI's own environment, which the
/// returned map holds. That fallback refuses the names that steer the CLI
/// ([`steers_the_cli`]). A name that the env file cannot hold (whitespace,
/// a leading `#` or BOM) and a single-line value over
/// [`ENV_FILE_LINE_MAX`] are refused.
fn push_env(
    args: &mut Vec<String>,
    env: &BTreeMap<String, String>,
    env_dir: &std::path::Path,
) -> Result<(BTreeMap<String, String>, Option<PrivateFile>), BuildError> {
    let mut file = String::new();
    let mut cli_env = BTreeMap::new();
    for (k, v) in env {
        env_key(k)?;
        if k.starts_with('#') || k.starts_with('\u{feff}') || k.chars().any(char::is_whitespace) {
            return Err(BuildError::bad(format!(
                "environment name {k:?} has whitespace or starts with '#' or a BOM; \
                 a docker env file cannot hold it"
            )));
        }
        if v.contains(['\n', '\r']) {
            if steers_the_cli(k) {
                return Err(BuildError::bad(format!(
                    "environment {k:?} has a multi-line value; it would go into the env of the \
                     host's docker CLI, and {k} steers the CLI or its loader"
                )));
            }
            cli_env.insert(k.clone(), v.clone());
        } else if k.len() + 1 + v.len() > ENV_FILE_LINE_MAX {
            return Err(BuildError::bad(format!(
                "environment {k:?} is over {ENV_FILE_LINE_MAX} bytes on one line; \
                 a docker env file cannot hold it"
            )));
        } else {
            file.push_str(k);
            file.push('=');
            file.push_str(v);
            file.push('\n');
        }
    }
    let private = if file.is_empty() {
        None
    } else {
        let f = PrivateFile::new(env_dir, ENV_FILE_PREFIX, file.as_bytes()).map_err(|e| {
            BuildError {
                code: crate::protocol::ErrorCode::SpawnFailed,
                message: format!("cannot write the env file in {}: {e}", env_dir.display()),
            }
        })?;
        let path = f
            .path()
            .to_str()
            .ok_or_else(|| BuildError::bad("the env file path is not UTF-8"))?
            .to_owned();
        args.push(s("--env-file"));
        args.push(path);
        Some(f)
    };
    for k in cli_env.keys() {
        args.push(s("-e"));
        args.push(k.clone());
    }
    Ok((cli_env, private))
}

fn push_labels(
    args: &mut Vec<String>,
    flag: &str,
    labels: &BTreeMap<String, String>,
) -> Result<(), BuildError> {
    for (k, v) in labels {
        ident("label", k)?;
        if k.contains('=') {
            return Err(BuildError::bad(format!("label key {k:?} contains '='")));
        }
        args.push(s(flag));
        args.push(format!("{k}={v}"));
    }
    Ok(())
}

/// `docker run`. Each entry of `workspaces` becomes
/// `-v <work_dir>/<path>:/work/<path>` ([`crate::workspace::resolve`]).
/// With a workspace and no `user`, the container runs as the owner of the
/// first workspace (`-u <uid>:<gid>`, with `HOME=/tmp`), so the files that
/// a build writes stay the agent's: the sandbox runs as that user too.
pub fn plan_run(
    req: &DockerRun,
    bin: &str,
    policy: &PathPolicy,
    work_dir: &std::path::Path,
    env_dir: &std::path::Path,
) -> Result<VerbPlan, BuildError> {
    let mut a = vec![s("run")];
    if req.detach {
        a.push(s("-d"));
        if req.pty.is_some() {
            return Err(BuildError::bad("docker.run with detach cannot have a pty"));
        }
    } else {
        a.push(s("-i"));
        if req.pty.is_some() {
            a.push(s("-t"));
        }
    }
    if req.rm {
        a.push(s("--rm"));
    }
    if req.init {
        a.push(s("--init"));
    }
    if let Some(l) = &req.reservation {
        l.validate().map_err(BuildError::bad)?;
        a.extend(limit_flags(&l.limits));
        a.extend(reservation_flags(l, std::path::Path::new("/dev/block"))?);
    } else if let Some(l) = &req.limits {
        a.extend(limit_flags(l));
    }
    if let Some(name) = &req.name {
        a.push(s("--name"));
        a.push(ident("name", name)?.to_owned());
    }
    let mut sources = Vec::new();
    for w in &req.workspaces {
        sources.push(crate::workspace::resolve(work_dir, w)?);
    }
    let mut req_env = req.env.clone();
    // Override image defaults and caller env. Docker mounts only this allowlist.
    let devices = req
        .reservation
        .as_ref()
        .map(|r| r.gpu_devices.as_slice())
        .unwrap_or_default();
    req_env.insert(
        "NVIDIA_VISIBLE_DEVICES".into(),
        if devices.is_empty() {
            "void".into()
        } else {
            devices.join(",")
        },
    );
    if !devices.is_empty() {
        a.push("--gpus".into());
        a.push(format!("\"device={}\"", devices.join(",")));
    }
    let mut user = req.user.clone();
    if user.is_none()
        && let Some(first) = sources.first()
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(first)
            .map_err(|e| BuildError::path(format!("cannot stat the workspace: {e}")))?;
        user = Some(format!("{}:{}", m.uid(), m.gid()));
        req_env
            .entry("HOME".into())
            .or_insert_with(|| "/tmp".into());
    }
    let (env, env_file) = push_env(&mut a, &req_env, env_dir)?;
    for v in &req.volumes {
        if v.source.is_empty()
            || v.target.is_empty()
            || v.source.contains(':')
            || v.target.contains(':')
        {
            return Err(BuildError::bad(format!(
                "bad volume {}:{}",
                v.source, v.target
            )));
        }
        if v.source.starts_with('/') {
            policy
                .check(std::path::Path::new(&v.source))
                .map_err(BuildError::path)?;
        } else {
            ident("volume", &v.source)?;
        }
        a.push(s("-v"));
        a.push(if v.read_only {
            format!("{}:{}:ro", v.source, v.target)
        } else {
            format!("{}:{}", v.source, v.target)
        });
    }
    for (w, source) in req.workspaces.iter().zip(&sources) {
        let source = source
            .to_str()
            .filter(|p| !p.contains(':'))
            .ok_or_else(|| BuildError::bad("the workspace path is not UTF-8 or has ':'"))?
            .to_owned();
        a.push(s("-v"));
        a.push(format!("{source}:{}", w.target()));
    }
    if let Some(w) = &req.workdir {
        a.push(s("-w"));
        a.push(ident("workdir", w)?.to_owned());
    }
    if let Some(n) = &req.network {
        a.push(s("--network"));
        a.push(ident("network", n)?.to_owned());
    }
    if let Some(u) = &user {
        a.push(s("-u"));
        a.push(ident("user", u)?.to_owned());
    }
    push_labels(&mut a, "--label", &req.labels)?;
    a.push(ident("image", &req.image)?.to_owned());
    a.extend(req.command.iter().cloned());
    let mut exec = base(bin, a, req.timeout_secs);
    exec.env = env;
    exec.private_file = env_file;
    exec.pty = req.pty;
    Ok(if req.detach {
        VerbPlan::parsed(exec, ResultParser::DockerRunId)
    } else {
        VerbPlan::plain(exec)
    })
}

/// The `docker run` flags of resource limits. `--memory-swap` is the
/// memory and the swap together, so with no `swap` the container gets no
/// swap, as on a `host` target. Docker's `--blkio-weight` is 10 to 1000;
/// the IO weight (1 to 10000) maps to it with the inverse of the formula
/// that runc uses on cgroup v2.
pub fn limit_flags(l: &Limits) -> Vec<String> {
    let mut a = Vec::new();
    if let Some(m) = l.cpu_millis {
        a.push(format!("--cpus={}", terrarium_core::limits::show_cores(m)));
    }
    if let Some(m) = l.memory_bytes {
        a.push(format!("--memory={m}"));
        let swap = l.swap_bytes.unwrap_or(0);
        a.push(format!("--memory-swap={}", m.saturating_add(swap)));
    }
    if let Some(p) = l.pids {
        a.push(format!("--pids-limit={p}"));
    }
    if let Some(w) = l.io_weight {
        a.push(format!("--blkio-weight={}", blkio_weight(w)));
    }
    a
}

/// Create-only device IO flags and CPU affinity. Device IDs were resolved on
/// this host, so Docker can use the kernel's /dev/block symlinks.
pub fn reservation_flags(
    l: &terrarium_core::resources::RuntimeLimits,
    device_root: &std::path::Path,
) -> Result<Vec<String>, BuildError> {
    l.validate().map_err(BuildError::bad)?;
    let mut args = Vec::new();
    if l.cpus.is_some() {
        args.push(format!("--cpuset-cpus={}", l.cpuset()));
    }
    for (device, io) in &l.io {
        // Docker splits device:rate at the colon, so /dev/block/major:minor
        // cannot be passed literally. Resolve the host's kernel device link.
        let path = std::fs::canonicalize(device_root.join(device))
            .map_err(|e| BuildError::bad(format!("cannot resolve IO device {device}: {e}")))?;
        let path = path
            .to_str()
            .filter(|p| !p.contains(':'))
            .ok_or_else(|| BuildError::bad("Docker IO device path must be UTF-8 without colons"))?;
        for (flag, value) in [
            ("read-bps", io.read_bps),
            ("write-bps", io.write_bps),
            ("read-iops", io.read_iops),
            ("write-iops", io.write_iops),
        ] {
            if value > 0 {
                args.push(format!("--device-{flag}={path}:{value}"));
            }
        }
    }
    Ok(args)
}

/// Docker update cannot apply per-device IO. Refuse it rather than silently
/// dropping the fields or recreating a container with other agents' processes.
pub fn plan_update(
    container: &str,
    l: &terrarium_core::resources::RuntimeLimits,
    bin: &str,
) -> Result<VerbPlan, BuildError> {
    l.validate().map_err(BuildError::bad)?;
    if !l.io.is_empty() {
        return Err(BuildError::bad(
            "device IO limits take effect at the next up; Docker cannot change them live",
        ));
    }
    if !l.gpu_devices.is_empty() {
        return Err(BuildError::bad(
            "GPU exposure takes effect at create; Docker cannot change it live",
        ));
    }
    if l.limits.cpu_millis.is_none() || l.limits.memory_bytes.is_none() || l.cpus.is_none() {
        return Err(BuildError::bad(
            "Docker live updates require explicit host-capacity defaults",
        ));
    }
    let mut a = vec![s("update")];
    a.extend(limit_flags(&l.limits));
    if l.limits.pids.is_none() {
        a.push("--pids-limit=-1".into());
    }
    if l.limits.io_weight.is_none() {
        a.push(format!("--blkio-weight={}", blkio_weight(100)));
    }
    a.push(format!("--cpuset-cpus={}", l.cpuset()));
    a.push(ident("container", container)?.to_owned());
    Ok(VerbPlan::plain(base(bin, a, Some(30))))
}

/// runc maps a blkio weight `b` (10 to 1000) to the cgroup v2 IO weight
/// `1 + (b - 10) * 9999 / 990`. This is the inverse, rounded.
pub fn blkio_weight(io_weight: u16) -> u16 {
    let w = u32::from(io_weight.clamp(1, 10_000));
    let b = 10 + ((w - 1) * 990 + 9999 / 2) / 9999;
    u16::try_from(b.clamp(10, 1000)).unwrap_or(1000)
}

pub fn plan_exec(
    req: &DockerExec,
    bin: &str,
    env_dir: &std::path::Path,
) -> Result<VerbPlan, BuildError> {
    let mut a = vec![s("exec"), s("-i")];
    if req.pty.is_some() {
        a.push(s("-t"));
    }
    if let Some(cwd) = &req.cwd {
        a.push(s("-w"));
        a.push(ident("cwd", cwd)?.to_owned());
    }
    let (env, env_file) = push_env(&mut a, &req.env, env_dir)?;
    if let Some(u) = &req.user {
        a.push(s("-u"));
        a.push(ident("user", u)?.to_owned());
    }
    a.push(ident("container", &req.container)?.to_owned());
    if req.cmd.is_empty() {
        return Err(BuildError::bad("cmd is empty"));
    }
    a.push(req.cmd.clone());
    a.extend(req.args.iter().cloned());
    let mut exec = base(bin, a, req.timeout_secs);
    exec.env = env;
    exec.private_file = env_file;
    exec.pty = req.pty;
    Ok(VerbPlan::plain(exec))
}

pub fn plan_start(req: &DockerStart, bin: &str) -> Result<VerbPlan, BuildError> {
    let a = vec![s("start"), ident("container", &req.container)?.to_owned()];
    Ok(VerbPlan::plain(base(bin, a, req.timeout_secs)))
}

pub fn plan_stop(req: &DockerStop, bin: &str) -> Result<VerbPlan, BuildError> {
    let mut a = vec![s("stop")];
    if let Some(g) = req.grace_secs {
        a.push(s("-t"));
        a.push(g.to_string());
    }
    a.push(ident("container", &req.container)?.to_owned());
    Ok(VerbPlan::plain(base(bin, a, req.timeout_secs)))
}

pub fn plan_rm(req: &DockerRm, bin: &str) -> Result<VerbPlan, BuildError> {
    let mut a = vec![s("rm")];
    if req.force {
        a.push(s("-f"));
    }
    if req.volumes {
        a.push(s("-v"));
    }
    a.push(ident("container", &req.container)?.to_owned());
    Ok(VerbPlan::plain(base(bin, a, req.timeout_secs)))
}

pub fn plan_inspect(
    req: &DockerInspect,
    bin: &str,
    work_dir: &std::path::Path,
) -> Result<VerbPlan, BuildError> {
    let a = vec![
        s("inspect"),
        s("--type"),
        s("container"),
        ident("container", &req.container)?.to_owned(),
    ];
    Ok(VerbPlan::parsed(
        base(bin, a, req.timeout_secs),
        ResultParser::DockerInspect(work_dir.to_owned()),
    ))
}

pub fn plan_ps(req: &DockerPs, bin: &str) -> Result<VerbPlan, BuildError> {
    let mut a = vec![s("ps"), s("--no-trunc"), s("--format"), s("json")];
    if req.all {
        a.push(s("-a"));
    }
    for (k, v) in &req.labels {
        ident("label", k)?;
        a.push(s("--filter"));
        a.push(format!("label={k}={v}"));
    }
    Ok(VerbPlan::parsed(
        base(bin, a, req.timeout_secs),
        ResultParser::DockerPs,
    ))
}

pub fn plan_volume_create(req: &DockerVolumeCreate, bin: &str) -> Result<VerbPlan, BuildError> {
    let mut a = vec![s("volume"), s("create")];
    push_labels(&mut a, "--label", &req.labels)?;
    a.push(ident("volume", &req.name)?.to_owned());
    Ok(VerbPlan::plain(base(bin, a, req.timeout_secs)))
}

pub fn plan_volume_rm(req: &DockerVolumeRm, bin: &str) -> Result<VerbPlan, BuildError> {
    let mut a = vec![s("volume"), s("rm")];
    if req.force {
        a.push(s("-f"));
    }
    a.push(ident("volume", &req.name)?.to_owned());
    Ok(VerbPlan::plain(base(bin, a, req.timeout_secs)))
}

// ------------------------------------------------------------------ parsers

/// `docker run -d` prints the container ID as its last stdout line.
pub fn parse_run_id(stdout: &str) -> Value {
    match stdout.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(id) => json!({ "id": id }),
        None => json!({ "id": null }),
    }
}

fn state_of(status: &str) -> TargetState {
    match status {
        "running" | "restarting" | "paused" => TargetState::Running,
        "created" | "exited" | "dead" => TargetState::Stopped,
        _ => TargetState::Unknown,
    }
}

/// `docker inspect --type container` prints a JSON array. A missing container
/// prints `[]` and exits 1.
pub fn parse_inspect(stdout: &str, code: Option<i32>) -> Value {
    let parsed: Option<Vec<Value>> = serde_json::from_str(stdout.trim()).ok();
    let Some(first) = parsed.as_ref().and_then(|v| v.first()) else {
        let state = if code == Some(0) {
            TargetState::Unknown
        } else {
            TargetState::Absent
        };
        return json!({ "state": state });
    };
    let st = &first["State"];
    let status = st["Status"].as_str().unwrap_or_default();
    json!({
        "state": state_of(status),
        "id": first["Id"],
        "name": first["Name"].as_str().map(|n| n.trim_start_matches('/')),
        "image": first["Config"]["Image"],
        "container_limits": first.get("HostConfig").map(|h| json!({
            "cpu": h["NanoCpus"], "cpu_quota": h["CpuQuota"], "cpu_period": h["CpuPeriod"],
            "memory": h["Memory"], "memory_swap": h["MemorySwap"],
            "pids": h["PidsLimit"], "blkio_weight": h["BlkioWeight"],
        })),
        "mounts": first["Mounts"],
        "status": status,
        "running": st["Running"],
        "exit_code": st["ExitCode"],
        "started_at": st["StartedAt"],
        "finished_at": st["FinishedAt"],
    })
}

/// Identify only exact workspace binds resolved against this host's work root.
pub fn parse_inspect_workspaces(
    stdout: &str,
    code: Option<i32>,
    work_dir: &std::path::Path,
) -> Value {
    let mut result = parse_inspect(stdout, code);
    let mut workspaces = serde_json::Map::new();
    if let Some(mounts) = result["mounts"].as_array() {
        for mount in mounts {
            let Some(path) = mount["Destination"]
                .as_str()
                .and_then(|s| s.strip_prefix("/work/"))
            else {
                continue;
            };
            let workspace = crate::protocol::WorkspaceMount {
                path: path.to_owned(),
            };
            if mount["Type"] == "bind"
                && mount["RW"] == true
                && mount["Propagation"] == "rprivate"
                && let Ok(source) = crate::workspace::resolve(work_dir, &workspace)
                && mount["Source"].as_str() == source.to_str()
            {
                workspaces.insert(path.to_owned(), mount.clone());
            }
        }
    }
    result["workspace_mounts"] = Value::Object(workspaces);
    result
}

/// `docker ps --format json` prints one JSON object per line.
pub fn parse_ps(stdout: &str) -> Value {
    let rows: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|r| {
            let labels: BTreeMap<String, String> = r["Labels"]
                .as_str()
                .unwrap_or_default()
                .split(',')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            let state = r["State"].as_str().unwrap_or_default();
            json!({
                "id": r["ID"],
                "names": r["Names"],
                "image": r["Image"],
                "state": state_of(state),
                "status": r["Status"],
                "labels": labels,
            })
        })
        .collect();
    Value::Array(rows)
}

/// Parse `docker version --format json`. Docker is present when the server
/// answered.
pub fn parse_version(stdout: &str) -> ToolCapability {
    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    match v["Server"]["Version"].as_str() {
        Some(ver) if !ver.is_empty() => ToolCapability {
            present: true,
            version: Some(ver.to_owned()),
        },
        _ => ToolCapability::default(),
    }
}

/// Run `docker version --format json`.
pub async fn detect(bin: &str) -> ToolCapability {
    match crate::verb::probe(bin, &["version", "--format", "json"]).await {
        Some((_, out)) => parse_version(&out),
        None => ToolCapability::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::VolumeMount;
    use std::path::PathBuf;

    /// The env dir of the tests.
    fn env_dir() -> PathBuf {
        std::env::temp_dir().join("terrarium-host-test-env")
    }

    fn plan_run(
        r: &DockerRun,
        bin: &str,
        policy: &PathPolicy,
        work: &std::path::Path,
    ) -> Result<VerbPlan, BuildError> {
        super::plan_run(r, bin, policy, work, &env_dir())
    }

    fn plan_exec(r: &DockerExec, bin: &str) -> Result<VerbPlan, BuildError> {
        super::plan_exec(r, bin, &env_dir())
    }

    fn run_req() -> DockerRun {
        serde_json::from_value(json!({"session":1,"image":"alpine:3"})).unwrap()
    }

    /// The env file of a plan, as a map.
    fn file_env(plan: &VerbPlan) -> BTreeMap<String, String> {
        let f = plan.exec.private_file.as_ref().expect("an env file");
        std::fs::read_to_string(f.path())
            .unwrap()
            .lines()
            .map(|l| {
                let (k, v) = l.split_once('=').unwrap();
                (k.to_owned(), v.to_owned())
            })
            .collect()
    }

    /// The args, with the env file path as `<env>`.
    fn args(plan: &VerbPlan) -> String {
        let mut a = plan.exec.args.join(" ");
        if let Some(f) = &plan.exec.private_file {
            a = a.replace(f.path().to_str().unwrap(), "<env>");
        }
        a
    }

    #[test]
    fn gpu_allowlist_overrides_caller_and_image_defaults() {
        let mut r = run_req();
        r.env.insert("NVIDIA_VISIBLE_DEVICES".into(), "all".into());
        let plan = plan_run(
            &r,
            "docker",
            &PathPolicy::default(),
            std::path::Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(file_env(&plan)["NVIDIA_VISIBLE_DEVICES"], "void");
        assert!(plan.exec.env.is_empty());
        assert!(!plan.exec.args.contains(&"--gpus".into()));
        r.reservation = Some(terrarium_core::resources::RuntimeLimits {
            gpu_devices: vec!["GPU-abcd".into(), "GPU-1234".into()],
            ..Default::default()
        });
        let plan = plan_run(
            &r,
            "docker",
            &PathPolicy::default(),
            std::path::Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(
            file_env(&plan)["NVIDIA_VISIBLE_DEVICES"],
            "GPU-abcd,GPU-1234"
        );
        assert!(
            plan.exec
                .args
                .windows(2)
                .any(|w| w == ["--gpus", "\"device=GPU-abcd,GPU-1234\""])
        );
        r.reservation.as_mut().unwrap().gpu_devices = vec!["all".into()];
        assert!(
            plan_run(
                &r,
                "docker",
                &PathPolicy::default(),
                std::path::Path::new("/nonexistent")
            )
            .is_err()
        );
    }

    #[test]
    fn run_golden() {
        let mut r = run_req();
        r.name = Some("t-buildbox".into());
        r.rm = true;
        r.init = true;
        r.env = BTreeMap::from([("TOKEN".into(), "secret".into())]);
        r.volumes = vec![
            VolumeMount {
                source: "/srv/work".into(),
                target: "/work".into(),
                read_only: false,
            },
            VolumeMount {
                source: "cache".into(),
                target: "/cache".into(),
                read_only: true,
            },
        ];
        r.workdir = Some("/work".into());
        r.network = Some("lab".into());
        r.user = Some("1000:1000".into());
        r.labels = BTreeMap::from([("terrarium.target".into(), "buildbox".into())]);
        r.command = vec!["sleep".into(), "infinity".into()];
        let plan = plan_run(
            &r,
            "docker",
            &PathPolicy::default(),
            std::path::Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(
            args(&plan),
            "run -d --rm --init --name t-buildbox --env-file <env> -v /srv/work:/work -v cache:/cache:ro \
             -w /work --network lab -u 1000:1000 --label terrarium.target=buildbox \
             alpine:3 sleep infinity"
        );
        assert_eq!(file_env(&plan)["TOKEN"], "secret");
        assert!(plan.exec.env.is_empty());
        assert!(!plan.exec.args.iter().any(|a| a.contains("secret")));
        assert_eq!(plan.parser, Some(ResultParser::DockerRunId));
    }

    #[test]
    fn run_maps_the_limits_to_flags() {
        let mut r = run_req();
        r.init = true;
        r.limits = Some(Limits {
            cpu_millis: Some(1500),
            memory_bytes: Some(4 << 30),
            swap_bytes: None,
            pids: Some(256),
            io_weight: Some(100),
        });
        let plan = plan_run(
            &r,
            "docker",
            &PathPolicy::default(),
            std::path::Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(
            args(&plan),
            "run -d --init --cpus=1.5 --memory=4294967296 --memory-swap=4294967296 \
             --pids-limit=256 --blkio-weight=20 --env-file <env> alpine:3"
        );
        // Swap comes on top of the memory.
        let swap = Limits {
            memory_bytes: Some(1 << 30),
            swap_bytes: Some(1 << 30),
            ..Default::default()
        };
        assert_eq!(
            limit_flags(&swap),
            ["--memory=1073741824", "--memory-swap=2147483648"]
        );
        assert!(limit_flags(&Limits::default()).is_empty());
        assert_eq!(blkio_weight(1), 10);
        assert_eq!(blkio_weight(10_000), 1000);
        // runc maps 500 to 1 + 490 * 9999 / 990 = 4950; that goes back to 500.
        assert_eq!(blkio_weight(4950), 500);
    }

    #[test]
    fn run_binds_the_workspaces_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(work.join("terra/w1")).unwrap();
        std::fs::create_dir_all(work.join("terra/w2")).unwrap();
        let mut r = run_req();
        r.workspaces = vec![
            crate::protocol::WorkspaceMount {
                path: "terra/w1".into(),
            },
            crate::protocol::WorkspaceMount {
                path: "terra/w2".into(),
            },
        ];
        let plan = plan_run(&r, "docker", &PathPolicy::default(), &work).unwrap();
        let real = std::fs::canonicalize(&work).unwrap();
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(work.join("terra/w1")).unwrap();
        assert_eq!(
            args(&plan),
            format!(
                "run -d --env-file <env> -v {w}/terra/w1:/work/terra/w1 -v {w}/terra/w2:/work/terra/w2 \
                 -u {u}:{g} alpine:3",
                w = real.display(),
                u = m.uid(),
                g = m.gid()
            )
        );
        // HOME is the container's: the CLI keeps the host agent's HOME.
        assert_eq!(file_env(&plan)["HOME"], "/tmp");
        assert!(plan.exec.env.is_empty());
        // A `user` of the frame wins.
        r.user = Some("0:0".into());
        let plan = plan_run(&r, "docker", &PathPolicy::default(), &work).unwrap();
        assert!(plan.exec.args.join(" ").contains("-u 0:0 alpine:3"));
        assert!(!file_env(&plan).contains_key("HOME"));
        r.user = None;
        r.workspaces = vec![crate::protocol::WorkspaceMount {
            path: "terra/nobody".into(),
        }];
        let err = plan_run(&r, "docker", &PathPolicy::default(), &work).unwrap_err();
        assert_eq!(err.code, crate::protocol::ErrorCode::PathDenied);
        assert!(
            err.message.contains("TERRARIUM_WORK_DIR"),
            "{}",
            err.message
        );
    }

    #[test]
    fn attached_run_with_pty() {
        let mut r = run_req();
        r.detach = false;
        r.pty = Some(Default::default());
        let plan = plan_run(
            &r,
            "docker",
            &PathPolicy::default(),
            std::path::Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(args(&plan), "run -i -t --env-file <env> alpine:3");
        assert!(plan.exec.pty.is_some());
        assert_eq!(plan.parser, None);
    }

    #[test]
    fn run_refuses_flag_injection_and_bind_mounts_outside_roots() {
        let mut r = run_req();
        r.image = "--privileged".into();
        assert!(
            plan_run(
                &r,
                "docker",
                &PathPolicy::default(),
                std::path::Path::new("/nonexistent")
            )
            .is_err()
        );
        let mut r = run_req();
        r.volumes = vec![VolumeMount {
            source: "/etc".into(),
            target: "/x".into(),
            read_only: false,
        }];
        let policy = PathPolicy {
            roots: Some(vec![PathBuf::from("/srv/nothing-here")]),
        };
        let err =
            plan_run(&r, "docker", &policy, std::path::Path::new("/nonexistent")).unwrap_err();
        assert_eq!(err.code, crate::protocol::ErrorCode::PathDenied);
    }

    fn exec_req(env: serde_json::Value) -> DockerExec {
        serde_json::from_value(json!({
            "session":1,"container":"c1","cmd":"sh","args":["-c","id"],"env":env
        }))
        .unwrap()
    }

    #[test]
    fn exec_env_stays_out_of_the_cli_env() {
        let plan = plan_exec(
            &exec_req(json!({
                "DOCKER_HOST":"unix:///tmp/terrarium-jobs/j1/docker.sock",
                "DOCKER_CONTEXT":"job","DOCKER_CONFIG":"/tmp/cfg","A":"1 two=3"
            })),
            "docker",
        )
        .unwrap();
        assert!(plan.exec.env.is_empty(), "{:?}", plan.exec.env);
        assert_eq!(args(&plan), "exec -i --env-file <env> c1 sh -c id");
        let env = file_env(&plan);
        assert_eq!(
            env["DOCKER_HOST"],
            "unix:///tmp/terrarium-jobs/j1/docker.sock"
        );
        assert_eq!(env["DOCKER_CONTEXT"], "job");
        assert_eq!(env["DOCKER_CONFIG"], "/tmp/cfg");
        assert_eq!(env["A"], "1 two=3");
        let path = plan.exec.private_file.as_ref().unwrap().path().to_owned();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(plan);
        assert!(!path.exists());
        // No env, no file.
        let plan = plan_exec(&exec_req(json!({})), "docker").unwrap();
        assert!(plan.exec.private_file.is_none());
        assert_eq!(args(&plan), "exec -i c1 sh -c id");
    }

    #[test]
    fn only_multi_line_values_fall_back_to_the_cli_env() {
        let plan = plan_exec(
            &exec_req(json!({"KEY":"a\nb","CR":"a\rb","B":"1"})),
            "docker",
        )
        .unwrap();
        assert_eq!(
            args(&plan),
            "exec -i --env-file <env> -e CR -e KEY c1 sh -c id"
        );
        assert_eq!(plan.exec.env["KEY"], "a\nb");
        assert_eq!(plan.exec.env["CR"], "a\rb");
        assert_eq!(file_env(&plan).len(), 1);
        // A name that steers the CLI or its loader never goes into the CLI env.
        for k in [
            "DOCKER_HOST",
            "HOME",
            "PATH",
            "https_proxy",
            "FTP_PROXY",
            "LD_PRELOAD",
            "LD_AUDIT",
            "SSL_CERT_FILE",
            "GODEBUG",
            "GCONV_PATH",
            "MALLOC_ARENA_MAX",
            "TMPDIR",
            "XDG_CONFIG_HOME",
            "PYTHONPATH",
            "BASH_ENV",
            "SSH_ASKPASS",
            "SSH_AUTH_SOCK",
            "NODE_OPTIONS",
            "GNUPGHOME",
            "PERL5OPT",
            "JAVA_TOOL_OPTIONS",
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "TZDIR",
            "NIS_PATH",
            "OPENSSL_CONF",
            "OPENSSL_MODULES",
            "OPENSSL_ENGINES",
            "KRB5_CONFIG",
            "KRB5CCNAME",
            "DBUS_SESSION_BUS_ADDRESS",
            "TERRARIUM_GIT_TRAILERS_FILE",
            "TERRARIUM_X",
        ] {
            for v in ["a\nb", "a\rb"] {
                let err = plan_exec(&exec_req(json!({ k: v })), "docker").unwrap_err();
                assert_eq!(err.code, crate::protocol::ErrorCode::BadRequest);
                assert!(err.message.contains(k), "{}", err.message);
            }
        }
    }

    #[test]
    fn a_long_line_or_a_name_the_file_cannot_hold_is_refused() {
        // A long single-line value is refused, never put in the CLI env:
        // the loader reads spaces as separators.
        let long = format!("/srv/work/x.so{}", " ".repeat(ENV_FILE_LINE_MAX));
        for k in ["LD_PRELOAD", "FOO"] {
            let err = plan_exec(&exec_req(json!({ k: long })), "docker").unwrap_err();
            assert_eq!(err.code, crate::protocol::ErrorCode::BadRequest);
            assert!(err.message.contains("bytes on one line"), "{}", err.message);
        }
        // Just under the limit goes into the file.
        let fits = "x".repeat(ENV_FILE_LINE_MAX - "FOO=".len());
        let plan = plan_exec(&exec_req(json!({ "FOO": fits })), "docker").unwrap();
        assert_eq!(file_env(&plan)["FOO"], fits);
        assert!(plan.exec.env.is_empty());
        for k in ["A B", "#A", "\u{feff}A", "A\tB"] {
            let err = plan_exec(&exec_req(json!({ k: "1" })), "docker").unwrap_err();
            assert_eq!(err.code, crate::protocol::ErrorCode::BadRequest);
        }
    }

    #[test]
    fn the_env_file_dir_is_private_and_a_start_removes_leftovers() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state/terrarium-host/env");
        let plan = super::plan_exec(&exec_req(json!({"A":"1"})), "docker", &dir).unwrap();
        let path = plan.exec.private_file.as_ref().unwrap().path().to_owned();
        assert_eq!(path.parent().unwrap(), dir);
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        // A crash leaves the file: forget the guard.
        std::mem::forget(plan);
        std::fs::write(dir.join("other"), "keep").unwrap();
        assert_eq!(
            crate::session::remove_private_files(&dir, ENV_FILE_PREFIX).unwrap(),
            1
        );
        assert!(!path.exists());
        assert!(dir.join("other").exists());
        // A missing dir is no error.
        assert_eq!(
            crate::session::remove_private_files(&tmp.path().join("none"), ENV_FILE_PREFIX)
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_symlinked_env_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("env");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = super::plan_exec(&exec_req(json!({"A":"1"})), "docker", &link).unwrap_err();
        assert_eq!(err.code, crate::protocol::ErrorCode::SpawnFailed);
        assert!(
            err.message.contains("not a dir of this user"),
            "{}",
            err.message
        );
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
        // A link in the parent is refused too.
        let err =
            super::plan_exec(&exec_req(json!({"A":"1"})), "docker", &link.join("sub")).unwrap_err();
        assert!(
            err.message.contains("not a dir of this user"),
            "{}",
            err.message
        );
    }

    #[test]
    fn the_cli_deny_set_is_the_grant_list_plus_the_host_names() {
        use terrarium_core::env_deny::{DENIED_ENV, DENIED_ENV_PREFIXES, DENIED_ENV_SUFFIXES};
        let mut names: Vec<String> = DENIED_ENV.iter().map(|n| n.to_string()).collect();
        names.extend(DENIED_ENV_PREFIXES.iter().map(|p| format!("{p}X")));
        names.extend(DENIED_ENV_SUFFIXES.iter().map(|p| format!("X{p}")));
        names.extend(CLI_DENIED_ENV.iter().map(|n| n.to_string()));
        names.extend(CLI_DENIED_ENV_PREFIXES.iter().map(|p| format!("{p}X")));
        names.extend(["http_proxy".into(), "FOO".into(), "DEPLOY_KEY".into()]);
        names.extend(
            [
                "OPENSSL_CONF",
                "KRB5_CONFIG",
                "KRB5CCNAME",
                "DBUS_SESSION_BUS_ADDRESS",
                "TERRARIUM_X",
            ]
            .map(String::from),
        );
        names.extend(CLI_ALLOWED_ENV.iter().map(|n| n.to_string()));
        for n in &names {
            let expected = (terrarium_core::env_deny::denied(n)
                && !CLI_ALLOWED_ENV.contains(&n.as_str()))
                || CLI_DENIED_ENV.contains(&n.as_str())
                || CLI_DENIED_ENV_PREFIXES.iter().any(|p| n.starts_with(p))
                || n.to_ascii_uppercase().ends_with("_PROXY");
            assert_eq!(steers_the_cli(n), expected, "{n}");
        }
        assert!(!steers_the_cli("FOO") && !steers_the_cli("DEPLOY_KEY"));
        assert!(steers_the_cli("http_proxy") && steers_the_cli("SSH_ASKPASS"));
        // The OpenSSL, Kerberos and D-Bus names come from the grant list.
        for n in [
            "OPENSSL_CONF",
            "KRB5_CONFIG",
            "KRB5CCNAME",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            assert!(
                terrarium_core::env_deny::denied(n) && steers_the_cli(n),
                "{n}"
            );
        }
        // Only the exact names that the daemon sends multi-line go through,
        // not the whole TERRARIUM_ prefix.
        assert!(steers_the_cli("TERRARIUM_X") && steers_the_cli("TERRARIUM_GIT_TRAILERS_FILE"));
        for n in CLI_ALLOWED_ENV {
            assert!(
                terrarium_core::env_deny::denied(n) && !steers_the_cli(n),
                "{n}"
            );
        }
        let plan = plan_exec(
            &exec_req(json!({"TERRARIUM_GIT_TRAILERS":"Terrarium-Agent: a\nTerrarium-Card: b\n"})),
            "docker",
        )
        .unwrap();
        assert_eq!(args(&plan), "exec -i -e TERRARIUM_GIT_TRAILERS c1 sh -c id");
    }

    /// A fake `docker` CLI: it prints the DOCKER_* env that it runs with, the
    /// mode and content of its env file, and the value of each `-e` name.
    const FAKE_DOCKER: &str = r#"
echo "cli DOCKER_HOST=${DOCKER_HOST-unset} DOCKER_CONTEXT=${DOCKER_CONTEXT-unset}"
while [ $# -gt 0 ]; do
  case "$1" in
    --env-file) shift; stat -c %a "$1"; cat "$1" ;;
    -e) shift; eval "v=\${$1}"; printf 'via -e %s=%s\n' "$1" "$v" ;;
  esac
  shift
done
"#;

    #[tokio::test]
    async fn the_cli_keeps_its_own_docker_env_and_the_command_gets_its_values() {
        let mut plan = plan_exec(
            &exec_req(json!({
                "DOCKER_HOST":"unix:///tmp/job.sock","DOCKER_CONTEXT":"job",
                "FOO":"line 1\nline 2"
            })),
            "docker",
        )
        .unwrap();
        // Run the fake CLI with `sh -c`, so no executable file is written.
        plan.exec.program = "sh".into();
        plan.exec
            .args
            .splice(0..0, ["-c".into(), FAKE_DOCKER.into(), "docker".into()]);
        plan.exec.capture_stdout = true;
        let path = plan.exec.private_file.as_ref().unwrap().path().to_owned();
        let ob = std::sync::Arc::new(crate::outbox::Outbox::new(1, 1 << 20));
        let run = crate::session::spawn(plan.exec, ob).unwrap();
        let info = run.done.await.unwrap();
        assert_eq!(info.code, Some(0));
        let out = String::from_utf8(info.stdout).unwrap();
        let own = |k: &str| std::env::var(k).unwrap_or_else(|_| "unset".into());
        assert_eq!(
            out,
            format!(
                "cli DOCKER_HOST={} DOCKER_CONTEXT={}\n600\n\
                 DOCKER_CONTEXT=job\nDOCKER_HOST=unix:///tmp/job.sock\n\
                 via -e FOO=line 1\nline 2\n",
                own("DOCKER_HOST"),
                own("DOCKER_CONTEXT")
            )
        );
        // The session removed the file after the CLI exited.
        assert!(!path.exists());
    }

    #[test]
    fn other_verbs_golden() {
        let exec: DockerExec = serde_json::from_value(json!({
            "session":1,"container":"c1","cmd":"sh","args":["-c","id"],
            "cwd":"/work","env":{"A":"1"},"user":"root","pty":{"rows":10,"cols":20}
        }))
        .unwrap();
        assert_eq!(
            args(&plan_exec(&exec, "docker").unwrap()),
            "exec -i -t -w /work --env-file <env> -u root c1 sh -c id"
        );
        let stop: DockerStop =
            serde_json::from_value(json!({"session":1,"container":"c1","grace_secs":5})).unwrap();
        assert_eq!(
            plan_stop(&stop, "docker").unwrap().exec.args,
            ["stop", "-t", "5", "c1"]
        );
        let start: DockerStart =
            serde_json::from_value(json!({"session":1,"container":"c1"})).unwrap();
        assert_eq!(
            plan_start(&start, "docker").unwrap().exec.args,
            ["start", "c1"]
        );
        let bad: DockerStart =
            serde_json::from_value(json!({"session":1,"container":"-x"})).unwrap();
        assert!(plan_start(&bad, "docker").is_err());
        let rm: DockerRm = serde_json::from_value(
            json!({"session":1,"container":"c1","force":true,"volumes":true}),
        )
        .unwrap();
        assert_eq!(
            plan_rm(&rm, "docker").unwrap().exec.args,
            ["rm", "-f", "-v", "c1"]
        );
        let ins: DockerInspect =
            serde_json::from_value(json!({"session":1,"container":"c1"})).unwrap();
        let p = plan_inspect(&ins, "/usr/bin/docker", std::path::Path::new("/work")).unwrap();
        assert_eq!(p.exec.program, "/usr/bin/docker");
        assert_eq!(p.exec.args, ["inspect", "--type", "container", "c1"]);
        assert!(p.exec.capture_stdout);
        let ps: DockerPs = serde_json::from_value(
            json!({"session":1,"all":true,"labels":{"terrarium.host":"lab1"}}),
        )
        .unwrap();
        assert_eq!(
            plan_ps(&ps, "docker").unwrap().exec.args.join(" "),
            "ps --no-trunc --format json -a --filter label=terrarium.host=lab1"
        );
        let vc: DockerVolumeCreate = serde_json::from_value(
            json!({"session":1,"name":"work","labels":{"terrarium.agent":"a1"}}),
        )
        .unwrap();
        assert_eq!(
            plan_volume_create(&vc, "docker")
                .unwrap()
                .exec
                .args
                .join(" "),
            "volume create --label terrarium.agent=a1 work"
        );
        let vr: DockerVolumeRm =
            serde_json::from_value(json!({"session":1,"name":"work","force":true})).unwrap();
        assert_eq!(
            plan_volume_rm(&vr, "docker").unwrap().exec.args,
            ["volume", "rm", "-f", "work"]
        );
    }

    #[test]
    fn parses_inspect() {
        let out = r#"[{"Id":"abc123","Name":"/t-buildbox","Config":{"Image":"alpine:3"},
            "State":{"Status":"running","Running":true,"ExitCode":0,
            "StartedAt":"2026-10-24T01:02:03Z","FinishedAt":"0001-01-01T00:00:00Z"}}]"#;
        let v = parse_inspect(out, Some(0));
        assert_eq!(v["state"], "running");
        assert_eq!(v["name"], "t-buildbox");
        assert_eq!(v["id"], "abc123");
        assert!(v["mounts"].is_null());
        let mounted = out.replace("\"State\":", "\"Mounts\":[{\"Type\":\"volume\",\"Name\":\"data\",\"Destination\":\"/data\"}],\"State\":");
        assert_eq!(
            parse_inspect(&mounted, Some(0))["mounts"][0]["Name"],
            "data"
        );
        assert_eq!(parse_inspect("[]\n", Some(1))["state"], "absent");
        let exited = out.replace("\"running\"", "\"exited\"");
        assert_eq!(parse_inspect(&exited, Some(0))["state"], "stopped");
    }

    #[test]
    fn parses_ps_lines() {
        let out = concat!(
            r#"{"Command":"\"sleep infinity\"","ID":"abc","Image":"alpine:3","Labels":"terrarium.target=buildbox,x=y","Names":"t-buildbox","State":"running","Status":"Up 2 minutes"}"#,
            "\n",
            r#"{"ID":"def","Image":"busybox","Labels":"","Names":"old","State":"exited","Status":"Exited (0) 1 hour ago"}"#,
            "\n"
        );
        let v = parse_ps(out);
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["labels"]["terrarium.target"], "buildbox");
        assert_eq!(v[0]["state"], "running");
        assert_eq!(v[1]["state"], "stopped");
        assert_eq!(parse_ps(""), json!([]));
    }

    #[test]
    fn parses_run_id_and_version() {
        assert_eq!(parse_run_id("abc123\n")["id"], "abc123");
        // Recorded from `docker version --format json` (Docker 29.8.0).
        let out = r#"{"Client":{"Version":"29.8.0","ApiVersion":"1.56","Os":"linux","Arch":"amd64"},"Server":{"Platform":{"Name":""},"Version":"29.8.0","ApiVersion":"1.56","MinAPIVersion":"1.40","Os":"linux","Arch":"amd64"}}"#;
        let cap = parse_version(out);
        assert!(cap.present);
        assert_eq!(cap.version.as_deref(), Some("29.8.0"));
        // Client only: the daemon is not reachable.
        let client_only = r#"{"Client":{"Version":"29.8.0"},"Server":null}"#;
        assert!(!parse_version(client_only).present);
        assert!(!parse_version("").present);
    }
}
