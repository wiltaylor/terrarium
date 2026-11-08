//! `docker.*` verbs: command builders and output parsers.
//!
//! Each verb becomes one `docker` CLI process, run as an exec session.
//! Environment values never go on the command line: the builder passes
//! `-e KEY` and sets `KEY` in the CLI's own environment, so the values do not
//! show in `ps`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::config::PathPolicy;
use crate::protocol::{
    DockerExec, DockerInspect, DockerPs, DockerRm, DockerRun, DockerStart, DockerStop,
    DockerVolumeCreate, DockerVolumeRm, Limits, TargetState, ToolCapability,
};
use crate::session::ExecPlan;
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

fn push_env(
    args: &mut Vec<String>,
    env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, BuildError> {
    for k in env.keys() {
        env_key(k)?;
        args.push(s("-e"));
        args.push(k.clone());
    }
    Ok(env.clone())
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
    if let Some(l) = &req.limits {
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
    let env = push_env(&mut a, &req_env)?;
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

/// runc maps a blkio weight `b` (10 to 1000) to the cgroup v2 IO weight
/// `1 + (b - 10) * 9999 / 990`. This is the inverse, rounded.
pub fn blkio_weight(io_weight: u16) -> u16 {
    let w = u32::from(io_weight.clamp(1, 10_000));
    let b = 10 + ((w - 1) * 990 + 9999 / 2) / 9999;
    u16::try_from(b.clamp(10, 1000)).unwrap_or(1000)
}

pub fn plan_exec(req: &DockerExec, bin: &str) -> Result<VerbPlan, BuildError> {
    let mut a = vec![s("exec"), s("-i")];
    if req.pty.is_some() {
        a.push(s("-t"));
    }
    if let Some(cwd) = &req.cwd {
        a.push(s("-w"));
        a.push(ident("cwd", cwd)?.to_owned());
    }
    let env = push_env(&mut a, &req.env)?;
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

pub fn plan_inspect(req: &DockerInspect, bin: &str) -> Result<VerbPlan, BuildError> {
    let a = vec![
        s("inspect"),
        s("--type"),
        s("container"),
        ident("container", &req.container)?.to_owned(),
    ];
    Ok(VerbPlan::parsed(
        base(bin, a, req.timeout_secs),
        ResultParser::DockerInspect,
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
        "mounts": first["Mounts"],
        "status": status,
        "running": st["Running"],
        "exit_code": st["ExitCode"],
        "started_at": st["StartedAt"],
        "finished_at": st["FinishedAt"],
    })
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

    fn run_req() -> DockerRun {
        serde_json::from_value(json!({"session":1,"image":"alpine:3"})).unwrap()
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
            plan.exec.args.join(" "),
            "run -d --rm --init --name t-buildbox -e TOKEN -v /srv/work:/work -v cache:/cache:ro \
             -w /work --network lab -u 1000:1000 --label terrarium.target=buildbox \
             alpine:3 sleep infinity"
        );
        assert_eq!(plan.exec.env["TOKEN"], "secret");
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
            plan.exec.args.join(" "),
            "run -d --init --cpus=1.5 --memory=4294967296 --memory-swap=4294967296 \
             --pids-limit=256 --blkio-weight=20 alpine:3"
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
            plan.exec.args.join(" "),
            format!(
                "run -d -e HOME -v {w}/terra/w1:/work/terra/w1 -v {w}/terra/w2:/work/terra/w2 \
                 -u {u}:{g} alpine:3",
                w = real.display(),
                u = m.uid(),
                g = m.gid()
            )
        );
        assert_eq!(plan.exec.env["HOME"], "/tmp");
        // A `user` of the frame wins.
        r.user = Some("0:0".into());
        let plan = plan_run(&r, "docker", &PathPolicy::default(), &work).unwrap();
        assert!(plan.exec.args.join(" ").contains("-u 0:0 alpine:3"));
        assert!(!plan.exec.args.contains(&"HOME".to_owned()));
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
        assert_eq!(plan.exec.args, ["run", "-i", "-t", "alpine:3"]);
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

    #[test]
    fn other_verbs_golden() {
        let exec: DockerExec = serde_json::from_value(json!({
            "session":1,"container":"c1","cmd":"sh","args":["-c","id"],
            "cwd":"/work","env":{"A":"1"},"user":"root","pty":{"rows":10,"cols":20}
        }))
        .unwrap();
        assert_eq!(
            plan_exec(&exec, "docker").unwrap().exec.args.join(" "),
            "exec -i -t -w /work -e A -u root c1 sh -c id"
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
        let p = plan_inspect(&ins, "/usr/bin/docker").unwrap();
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
