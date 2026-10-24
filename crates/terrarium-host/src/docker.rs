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
    DockerExec, DockerInspect, DockerPs, DockerRm, DockerRun, DockerStop, DockerVolumeCreate,
    DockerVolumeRm, TargetState, ToolCapability,
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

pub fn plan_run(req: &DockerRun, bin: &str, policy: &PathPolicy) -> Result<VerbPlan, BuildError> {
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
    if let Some(name) = &req.name {
        a.push(s("--name"));
        a.push(ident("name", name)?.to_owned());
    }
    let env = push_env(&mut a, &req.env)?;
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
    if let Some(w) = &req.workdir {
        a.push(s("-w"));
        a.push(ident("workdir", w)?.to_owned());
    }
    if let Some(n) = &req.network {
        a.push(s("--network"));
        a.push(ident("network", n)?.to_owned());
    }
    if let Some(u) = &req.user {
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
        let plan = plan_run(&r, "docker", &PathPolicy::default()).unwrap();
        assert_eq!(
            plan.exec.args.join(" "),
            "run -d --rm --name t-buildbox -e TOKEN -v /srv/work:/work -v cache:/cache:ro \
             -w /work --network lab -u 1000:1000 --label terrarium.target=buildbox \
             alpine:3 sleep infinity"
        );
        assert_eq!(plan.exec.env["TOKEN"], "secret");
        assert!(!plan.exec.args.iter().any(|a| a.contains("secret")));
        assert_eq!(plan.parser, Some(ResultParser::DockerRunId));
    }

    #[test]
    fn attached_run_with_pty() {
        let mut r = run_req();
        r.detach = false;
        r.pty = Some(Default::default());
        let plan = plan_run(&r, "docker", &PathPolicy::default()).unwrap();
        assert_eq!(plan.exec.args, ["run", "-i", "-t", "alpine:3"]);
        assert!(plan.exec.pty.is_some());
        assert_eq!(plan.parser, None);
    }

    #[test]
    fn run_refuses_flag_injection_and_bind_mounts_outside_roots() {
        let mut r = run_req();
        r.image = "--privileged".into();
        assert!(plan_run(&r, "docker", &PathPolicy::default()).is_err());
        let mut r = run_req();
        r.volumes = vec![VolumeMount {
            source: "/etc".into(),
            target: "/x".into(),
            read_only: false,
        }];
        let policy = PathPolicy {
            roots: Some(vec![PathBuf::from("/srv/nothing-here")]),
        };
        let err = plan_run(&r, "docker", &policy).unwrap_err();
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
