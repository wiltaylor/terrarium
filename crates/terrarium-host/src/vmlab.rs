//! `vmlab.*` verbs: command builders and output parsers.
//!
//! vmlab finds its lab by walking up from the current directory to a
//! `vmlab.wcl`, the way git finds a repository. So every builder runs the
//! CLI with `cwd` set to the lab directory, and names machines by their bare
//! name. The CLI reference is the `vmlab` skill (`cli-lab.md`,
//! `cli-machine.md`); the flags here come from it.
//!
//! `vmlab status` and `vmlab snapshot list` have no JSON output, so the
//! parsers read their tables by the header column positions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::config::PathPolicy;
use crate::protocol::{
    CpDirection, TargetState, ToolCapability, VmlabCp, VmlabExec, VmlabLifecycle, VmlabShell,
    VmlabSnapshot, VmlabSnapshotDelete, VmlabSnapshotList, VmlabStatus,
};
use crate::session::ExecPlan;
use crate::verb::{BuildError, ResultParser, VerbPlan, ident, secs};

/// The lab file name vmlab looks for.
pub const LAB_FILE: &str = "vmlab.wcl";

/// Extra seconds the host agent waits past `vmlab exec --timeout`, so vmlab
/// reports its own timeout first.
const EXEC_TIMEOUT_SLACK: u64 = 30;

/// The lab directory for a `lab` field: a directory, or the path of its
/// `vmlab.wcl`. Checked against the path policy.
pub fn lab_dir(lab: &Path, policy: &PathPolicy) -> Result<PathBuf, BuildError> {
    let dir = match lab.file_name().and_then(|n| n.to_str()) {
        Some(LAB_FILE) => lab
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| BuildError::bad("lab has no parent directory"))?,
        Some(name) if name.ends_with(".wcl") => {
            return Err(BuildError::bad(format!(
                "lab {}: vmlab only finds a lab file named {LAB_FILE}; give the lab directory",
                lab.display()
            )));
        }
        _ => lab.to_path_buf(),
    };
    policy.check(&dir).map_err(BuildError::path)
}

fn s(v: &str) -> String {
    v.to_owned()
}

fn plan(bin: &str, dir: PathBuf, args: Vec<String>, timeout: Option<u64>) -> ExecPlan {
    ExecPlan {
        program: bin.to_owned(),
        args,
        cwd: Some(dir),
        timeout: secs(timeout),
        ..Default::default()
    }
}

fn machines(args: &mut Vec<String>, list: &[String]) -> Result<(), BuildError> {
    for m in list {
        args.push(ident("machine", m)?.to_owned());
    }
    Ok(())
}

pub fn plan_up(
    req: &VmlabLifecycle,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let mut a = vec![s("up")];
    machines(&mut a, &req.machines)?;
    Ok(VerbPlan::plain(plan(bin, dir, a, req.timeout_secs)))
}

pub fn plan_down(
    req: &VmlabLifecycle,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let mut a = vec![s("down")];
    if req.force {
        a.push(s("--force"));
    }
    machines(&mut a, &req.machines)?;
    Ok(VerbPlan::plain(plan(bin, dir, a, req.timeout_secs)))
}

pub fn plan_destroy(
    req: &VmlabLifecycle,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    if !req.machines.is_empty() {
        return Err(BuildError::bad(
            "vmlab destroy takes the whole lab; machines must be empty",
        ));
    }
    Ok(VerbPlan::plain(plan(
        bin,
        dir,
        vec![s("destroy")],
        req.timeout_secs,
    )))
}

pub fn plan_status(
    req: &VmlabStatus,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let mut a = vec![s("status")];
    if req.verbose {
        a.push(s("-v"));
    }
    Ok(VerbPlan::parsed(
        plan(bin, dir, a, req.timeout_secs),
        ResultParser::VmlabStatus,
    ))
}

pub fn plan_exec(req: &VmlabExec, bin: &str, policy: &PathPolicy) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let mut a = vec![s("exec")];
    if let Some(t) = req.timeout_secs.filter(|t| *t > 0) {
        a.push(s("--timeout"));
        a.push(t.to_string());
    }
    if let Some(u) = &req.user {
        a.push(s("--user"));
        a.push(ident("user", u)?.to_owned());
    }
    a.push(ident("machine", &req.machine)?.to_owned());
    if req.cmd.is_empty() {
        return Err(BuildError::bad("cmd is empty"));
    }
    a.push(s("--"));
    a.push(req.cmd.clone());
    a.extend(req.args.iter().cloned());
    let timeout = req
        .timeout_secs
        .filter(|t| *t > 0)
        .map(|t| t + EXEC_TIMEOUT_SLACK);
    Ok(VerbPlan::plain(plan(bin, dir, a, timeout)))
}

pub fn plan_shell(
    req: &VmlabShell,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let mut a = vec![s("shell")];
    if let Some(u) = &req.user {
        a.push(s("--user"));
        a.push(ident("user", u)?.to_owned());
    }
    a.push(ident("machine", &req.machine)?.to_owned());
    let mut p = plan(bin, dir, a, None);
    p.pty = Some(req.pty);
    Ok(VerbPlan::plain(p))
}

pub fn plan_cp(req: &VmlabCp, bin: &str, policy: &PathPolicy) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let host = policy.check(&req.host_path).map_err(BuildError::path)?;
    let host = host
        .to_str()
        .ok_or_else(|| BuildError::bad("host_path is not UTF-8"))?
        .to_owned();
    let machine = ident("machine", &req.machine)?;
    if req.guest_path.is_empty() || req.guest_path.chars().any(char::is_control) {
        return Err(BuildError::bad("bad guest_path"));
    }
    let guest = format!("{machine}:{}", req.guest_path);
    let a = match req.direction {
        CpDirection::Push => vec![s("cp"), host, guest],
        CpDirection::Pull => vec![s("cp"), guest, host],
    };
    Ok(VerbPlan::plain(plan(bin, dir, a, req.timeout_secs)))
}

pub fn plan_snapshot_take(
    req: &VmlabSnapshot,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let mut a = vec![s("snapshot"), s("create")];
    if let Some(m) = &req.machine {
        a.push(s("--vm"));
        a.push(ident("machine", m)?.to_owned());
    }
    a.push(ident("snapshot name", &req.name)?.to_owned());
    Ok(VerbPlan::plain(plan(bin, dir, a, req.timeout_secs)))
}

pub fn plan_snapshot_restore(
    req: &VmlabSnapshot,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let mut a = vec![s("snapshot"), s("restore")];
    if let Some(m) = &req.machine {
        a.push(s("--vm"));
        a.push(ident("machine", m)?.to_owned());
    }
    if req.discard_guest_changes {
        a.push(s("--discard-guest-changes"));
    }
    a.push(ident("snapshot name", &req.name)?.to_owned());
    Ok(VerbPlan::plain(plan(bin, dir, a, req.timeout_secs)))
}

pub fn plan_snapshot_list(
    req: &VmlabSnapshotList,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let a = vec![
        s("snapshot"),
        s("list"),
        ident("machine", &req.machine)?.to_owned(),
    ];
    Ok(VerbPlan::parsed(
        plan(bin, dir, a, req.timeout_secs),
        ResultParser::VmlabSnapshotList,
    ))
}

/// `vmlab snapshot delete MACHINE NAME`: vmlab takes both as positionals,
/// and has no lab-wide form.
pub fn plan_snapshot_delete(
    req: &VmlabSnapshotDelete,
    bin: &str,
    policy: &PathPolicy,
) -> Result<VerbPlan, BuildError> {
    let dir = lab_dir(&req.lab, policy)?;
    let a = vec![
        s("snapshot"),
        s("delete"),
        ident("machine", &req.machine)?.to_owned(),
        ident("snapshot name", &req.name)?.to_owned(),
    ];
    Ok(VerbPlan::plain(plan(bin, dir, a, req.timeout_secs)))
}

// ------------------------------------------------------------------ parsers

/// Column start positions (in chars) of a table header.
fn columns(header: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = header.chars().collect();
    let mut cols = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != ' ' && (i == 0 || chars[i - 1] == ' ') {
            let start = i;
            while i < chars.len() && chars[i] != ' ' {
                i += 1;
            }
            cols.push((start, chars[start..i].iter().collect()));
        } else {
            i += 1;
        }
    }
    cols
}

fn key(title: &str) -> String {
    title
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Slice one row by the header's column positions.
fn row(line: &str, cols: &[(usize, String)]) -> BTreeMap<String, String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = BTreeMap::new();
    for (i, (start, title)) in cols.iter().enumerate() {
        let end = cols
            .get(i + 1)
            .map_or(chars.len(), |c| c.0)
            .min(chars.len());
        let start = (*start).min(end);
        let cell: String = chars[start..end].iter().collect();
        out.insert(key(title), cell.trim().to_owned());
    }
    out
}

fn status_state(status: &str) -> TargetState {
    match status {
        "running" | "booting" | "starting" | "unhealthy" => TargetState::Running,
        "stopped" => TargetState::Stopped,
        s if s.starts_with("exited") => TargetState::Stopped,
        _ => TargetState::Unknown,
    }
}

fn lab_name(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("lab \"")?;
    Some(rest.split('"').next()?.to_owned())
}

/// Parse `vmlab status` (with or without `-v`).
pub fn parse_status(text: &str) -> Value {
    let mut lab = None;
    let mut running = true;
    let mut machines: Vec<Value> = Vec::new();
    let mut tables: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    let mut current: Option<(&'static str, Vec<(usize, String)>)> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            current = None;
            continue;
        }
        if trimmed.starts_with("lab \"") {
            lab = lab_name(trimmed);
            if trimmed.ends_with("not running") {
                running = false;
            }
            continue;
        }
        let first = trimmed.split_whitespace().next().unwrap_or_default();
        let section = match first {
            "NAME" if trimmed.contains("KIND") => Some("machines"),
            "DEV" => Some("dev"),
            "SEGMENT" => Some("segments"),
            "MACHINE" if trimmed.contains("PULLING") => Some("downloads"),
            _ => None,
        };
        if let Some(section) = section {
            current = Some((section, columns(line)));
            continue;
        }
        let Some((section, cols)) = &current else {
            continue;
        };
        if *section == "machines" {
            // `-v` adds a `key=value ...` line under each machine.
            if first.contains('=') {
                if let Some(Value::Object(m)) = machines.last_mut() {
                    let detail: BTreeMap<String, String> = trimmed
                        .split_whitespace()
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect();
                    m.insert("detail".into(), json!(detail));
                }
                continue;
            }
            let r = row(line, cols);
            let status = r.get("status").cloned().unwrap_or_default();
            let ip = r.get("ip").filter(|ip| ip.as_str() != "-").cloned();
            machines.push(json!({
                "name": r.get("name"),
                "kind": r.get("kind"),
                "status": status,
                "state": status_state(&status),
                "ip": ip,
                "template": r.get("template_image"),
            }));
        } else if first.contains('=') {
            continue;
        } else {
            tables
                .entry(section)
                .or_default()
                .push(json!(row(line, cols)));
        }
    }
    json!({
        "lab": lab,
        "running": running,
        "machines": machines,
        "segments": tables.remove("segments").unwrap_or_default(),
        "dev": tables.remove("dev").unwrap_or_default(),
        "downloads": tables.remove("downloads").unwrap_or_default(),
    })
}

/// Parse `vmlab snapshot list <vm>`.
pub fn parse_snapshot_list(text: &str) -> Value {
    let mut cols = None;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("no snapshots for") {
            continue;
        }
        match &cols {
            None if trimmed.starts_with("NAME") => cols = Some(columns(line)),
            None => {}
            Some(c) => out.push(json!(row(line, c))),
        }
    }
    Value::Array(out)
}

/// Parse `vmlab --version` (`vmlab 0.30.0-alpha`).
pub fn parse_version(stdout: &str) -> ToolCapability {
    let line = stdout.lines().next().unwrap_or_default().trim();
    match line.strip_prefix("vmlab ") {
        Some(v) if !v.trim().is_empty() => ToolCapability {
            present: true,
            version: Some(v.trim().to_owned()),
        },
        _ => ToolCapability::default(),
    }
}

/// Run `vmlab --version`.
pub async fn detect(bin: &str) -> ToolCapability {
    match crate::verb::probe(bin, &["--version"]).await {
        Some((Some(0), out)) => parse_version(&out),
        _ => ToolCapability::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lc(lab: &str) -> VmlabLifecycle {
        serde_json::from_value(json!({"session":1,"lab":lab})).unwrap()
    }

    const P: PathPolicy = PathPolicy { roots: None };

    #[test]
    fn lab_dir_accepts_a_dir_or_its_vmlab_wcl() {
        assert_eq!(
            lab_dir(Path::new("/labs/win"), &P).unwrap(),
            PathBuf::from("/labs/win")
        );
        assert_eq!(
            lab_dir(Path::new("/labs/win/vmlab.wcl"), &P).unwrap(),
            PathBuf::from("/labs/win")
        );
        assert!(lab_dir(Path::new("/labs/win11.wcl"), &P).is_err());
        assert!(lab_dir(Path::new("relative"), &P).is_err());
    }

    #[test]
    fn lifecycle_golden() {
        let mut up = lc("/labs/ad");
        up.machines = vec!["dc01".into(), "client01".into()];
        let p = plan_up(&up, "vmlab", &P).unwrap();
        assert_eq!(p.exec.args, ["up", "dc01", "client01"]);
        assert_eq!(p.exec.cwd.as_deref(), Some(Path::new("/labs/ad")));

        let mut down = lc("/labs/ad");
        down.force = true;
        down.machines = vec!["buildbox".into()];
        assert_eq!(
            plan_down(&down, "vmlab", &P).unwrap().exec.args,
            ["down", "--force", "buildbox"]
        );
        assert_eq!(
            plan_destroy(&lc("/labs/ad"), "vmlab", &P)
                .unwrap()
                .exec
                .args,
            ["destroy"]
        );
        let mut bad = lc("/labs/ad");
        bad.machines = vec!["--force".into()];
        assert!(plan_up(&bad, "vmlab", &P).is_err());

        let st: VmlabStatus =
            serde_json::from_value(json!({"session":1,"lab":"/labs/ad","verbose":true})).unwrap();
        let p = plan_status(&st, "vmlab", &P).unwrap();
        assert_eq!(p.exec.args, ["status", "-v"]);
        assert_eq!(p.parser, Some(ResultParser::VmlabStatus));
    }

    #[test]
    fn exec_shell_cp_golden() {
        let ex: VmlabExec = serde_json::from_value(json!({
            "session":1,"lab":"/labs/ad","machine":"dc01","cmd":"powershell",
            "args":["-NoProfile","-Command","Get-Service ntds"],"user":"admin","timeout_secs":600
        }))
        .unwrap();
        let p = plan_exec(&ex, "vmlab", &P).unwrap();
        assert_eq!(
            p.exec.args,
            [
                "exec",
                "--timeout",
                "600",
                "--user",
                "admin",
                "dc01",
                "--",
                "powershell",
                "-NoProfile",
                "-Command",
                "Get-Service ntds"
            ]
        );
        assert_eq!(p.exec.timeout, Some(std::time::Duration::from_secs(630)));

        let sh: VmlabShell = serde_json::from_value(json!({
            "session":1,"lab":"/labs/ad","machine":"nix01","pty":{"rows":50,"cols":200}
        }))
        .unwrap();
        let p = plan_shell(&sh, "vmlab", &P).unwrap();
        assert_eq!(p.exec.args, ["shell", "nix01"]);
        assert_eq!(p.exec.pty.map(|s| s.cols), Some(200));

        let push: VmlabCp = serde_json::from_value(json!({
            "session":1,"lab":"/labs/ad","machine":"dc01","direction":"push",
            "host_path":"/tmp/tools.zip","guest_path":"C:/tools/tools.zip"
        }))
        .unwrap();
        assert_eq!(
            plan_cp(&push, "vmlab", &P).unwrap().exec.args,
            ["cp", "/tmp/tools.zip", "dc01:C:/tools/tools.zip"]
        );
        let mut pull = push.clone();
        pull.direction = CpDirection::Pull;
        assert_eq!(
            plan_cp(&pull, "vmlab", &P).unwrap().exec.args,
            ["cp", "dc01:C:/tools/tools.zip", "/tmp/tools.zip"]
        );
    }

    #[test]
    fn snapshot_golden() {
        let mut snap: VmlabSnapshot = serde_json::from_value(
            json!({"session":1,"lab":"/labs/ad","name":"pre-upgrade","machine":"dc01"}),
        )
        .unwrap();
        assert_eq!(
            plan_snapshot_take(&snap, "vmlab", &P).unwrap().exec.args,
            ["snapshot", "create", "--vm", "dc01", "pre-upgrade"]
        );
        snap.discard_guest_changes = true;
        assert_eq!(
            plan_snapshot_restore(&snap, "vmlab", &P).unwrap().exec.args,
            [
                "snapshot",
                "restore",
                "--vm",
                "dc01",
                "--discard-guest-changes",
                "pre-upgrade"
            ]
        );
        snap.machine = None;
        snap.discard_guest_changes = false;
        assert_eq!(
            plan_snapshot_restore(&snap, "vmlab", &P).unwrap().exec.args,
            ["snapshot", "restore", "pre-upgrade"]
        );
        let list: VmlabSnapshotList =
            serde_json::from_value(json!({"session":1,"lab":"/labs/ad","machine":"dc01"})).unwrap();
        let p = plan_snapshot_list(&list, "vmlab", &P).unwrap();
        assert_eq!(p.exec.args, ["snapshot", "list", "dc01"]);
        assert_eq!(p.parser, Some(ResultParser::VmlabSnapshotList));
        assert_eq!(p.exec.cwd.as_deref(), Some(Path::new("/labs/ad")));
    }

    #[test]
    fn snapshot_delete_golden_and_refusals() {
        let del: VmlabSnapshotDelete = serde_json::from_value(
            json!({"session":1,"lab":"/labs/ad/vmlab.wcl","machine":"dc01","name":"pre-upgrade"}),
        )
        .unwrap();
        let p = plan_snapshot_delete(&del, "vmlab", &P).unwrap();
        assert_eq!(p.exec.args, ["snapshot", "delete", "dc01", "pre-upgrade"]);
        assert_eq!(p.exec.cwd.as_deref(), Some(Path::new("/labs/ad")));
        assert_eq!(p.parser, None);
        for (machine, name) in [
            ("--force", "x"),
            ("dc01", "--all"),
            ("", "x"),
            ("dc01", ""),
            ("dc01", "a\nb"),
        ] {
            let mut bad = del.clone();
            bad.machine = machine.into();
            bad.name = name.into();
            assert_eq!(
                plan_snapshot_delete(&bad, "vmlab", &P).unwrap_err().code,
                crate::protocol::ErrorCode::BadRequest,
                "{machine:?} {name:?}"
            );
        }
        // The path policy holds for the lab dir.
        let policy = PathPolicy {
            roots: Some(vec![PathBuf::from("/srv/labs")]),
        };
        assert_eq!(
            plan_snapshot_delete(&del, "vmlab", &policy)
                .unwrap_err()
                .code,
            crate::protocol::ErrorCode::PathDenied
        );
        let mut take: VmlabSnapshot =
            serde_json::from_value(json!({"session":1,"lab":"/labs/ad","name":"--x"})).unwrap();
        assert!(plan_snapshot_take(&take, "vmlab", &P).is_err());
        take.name = "ok".into();
        take.machine = Some("-m".into());
        assert!(plan_snapshot_restore(&take, "vmlab", &P).is_err());
    }

    #[test]
    fn parses_a_snapshot_list_with_several_rows() {
        let text = "NAME              KIND     TAKEN\nterrarium-clean   online   2026-10-26T08:00:00Z\npre-upgrade       offline  2026-10-26T09:30:00.5Z\n";
        assert_eq!(
            parse_snapshot_list(text),
            json!([
                {"name": "terrarium-clean", "kind": "online", "taken": "2026-10-26T08:00:00Z"},
                {"name": "pre-upgrade", "kind": "offline", "taken": "2026-10-26T09:30:00.5Z"}
            ])
        );
        assert_eq!(parse_snapshot_list(""), json!([]));
    }

    // Recorded from the vmlab reference (cli-lab.md, `vmlab status`).
    const STATUS: &str = r#"lab "ad-lab"

  NAME     KIND      STATUS   IP           TEMPLATE/IMAGE
  dc01     vm        running  10.10.0.10   x86_64/winsrv2022
  client01 vm        booting  -            x86_64/win11
  buildbox vm        stopped  -            x86_64/ubuntu

  SEGMENT SUBNET         GATEWAY    NAT/DHCP DROPPED    PEER
  corp    10.10.0.0/24   10.10.0.1  on/on    0          -
"#;

    #[test]
    fn parses_status() {
        let v = parse_status(STATUS);
        assert_eq!(v["lab"], "ad-lab");
        assert_eq!(v["running"], true);
        let m = v["machines"].as_array().unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0]["name"], "dc01");
        assert_eq!(m[0]["ip"], "10.10.0.10");
        assert_eq!(m[0]["state"], "running");
        assert_eq!(m[0]["template"], "x86_64/winsrv2022");
        assert_eq!(m[1]["ip"], Value::Null);
        assert_eq!(m[1]["state"], "running");
        assert_eq!(m[2]["state"], "stopped");
        assert_eq!(v["segments"][0]["subnet"], "10.10.0.0/24");
        assert_eq!(v["segments"][0]["nat_dhcp"], "on/on");
    }

    #[test]
    fn parses_verbose_status_and_exited_containers() {
        let text = r#"lab "mixed-lab"

  NAME     KIND       STATUS       IP          TEMPLATE/IMAGE
  web      container  exited (1)   -           nginx:1.27
    state=stopped ready=false cached=yes health=none exit=1
  nix01    vm         running      10.0.0.5    x86_64/nixos
    state=running ready=true cached=yes arch=x86_64 cpus=2
"#;
        let v = parse_status(text);
        let m = v["machines"].as_array().unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["status"], "exited (1)");
        assert_eq!(m[0]["state"], "stopped");
        assert_eq!(m[0]["detail"]["exit"], "1");
        assert_eq!(m[1]["detail"]["cpus"], "2");
    }

    #[test]
    fn parses_not_running() {
        let v = parse_status("lab \"ad-lab\": not running\n");
        assert_eq!(v["lab"], "ad-lab");
        assert_eq!(v["running"], false);
        assert_eq!(v["machines"], json!([]));
    }

    #[test]
    fn parses_snapshot_list() {
        // Recorded from the vmlab reference (cli-machine.md).
        let text = "NAME                     KIND     TAKEN\npre-upgrade              online   2026-09-02T09:14:31Z\n";
        let v = parse_snapshot_list(text);
        assert_eq!(
            v,
            json!([{"name":"pre-upgrade","kind":"online","taken":"2026-09-02T09:14:31Z"}])
        );
        assert_eq!(parse_snapshot_list("no snapshots for dc01\n"), json!([]));
    }

    // Recorded live from vmlab 0.33.3-alpha against
    // labs/terrarium-test (one idle container) on 2026-10-25.
    const LIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vmlab-0.33.3/");

    fn live(name: &str) -> String {
        std::fs::read_to_string(format!("{LIVE}{name}")).unwrap()
    }

    #[test]
    fn parses_live_vmlab_0_33_output() {
        // Narrow columns: vmlab sizes each column to its widest cell.
        let v = parse_status(&live("status-v-running.txt"));
        assert_eq!(v["lab"], "terrarium-test");
        assert_eq!(v["running"], true);
        let m = &v["machines"][0];
        assert_eq!(v["machines"].as_array().unwrap().len(), 1);
        assert_eq!(m["name"], "box");
        assert_eq!(m["kind"], "container");
        assert_eq!(m["status"], "running");
        assert_eq!(m["state"], "running");
        assert_eq!(m["ip"], Value::Null);
        assert_eq!(m["template"], "alpine:3.22");
        assert_eq!(m["detail"]["ready"], "yes");
        assert_eq!(m["detail"]["health"], "-");
        assert!(
            m["detail"]["digest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );

        // `vmlab up` returns while a container is still `starting`.
        let v = parse_status(&live("status-starting.txt"));
        assert_eq!(v["machines"][0]["status"], "starting");
        assert_eq!(v["machines"][0]["state"], "running");
        assert_eq!(v["machines"][0]["template"], "alpine:3.22");

        let v = parse_status(&live("status-not-running.txt"));
        assert_eq!(v["lab"], "terrarium-test");
        assert_eq!(v["running"], false);
        assert_eq!(v["machines"], json!([]));

        assert_eq!(
            parse_snapshot_list(&live("snapshot-list.txt")),
            json!([{"name": "clean", "kind": "online",
                    "taken": "2026-10-25T09:16:04.854844022Z"}])
        );
        assert_eq!(
            parse_snapshot_list(&live("snapshot-list-empty.txt")),
            json!([])
        );
        let cap = parse_version(&live("version.txt"));
        assert_eq!(cap.version.as_deref(), Some("0.33.3-alpha"));
    }

    #[test]
    fn parses_version() {
        // Recorded from `vmlab --version`.
        let cap = parse_version("vmlab 0.30.0-alpha\n");
        assert!(cap.present);
        assert_eq!(cap.version.as_deref(), Some("0.30.0-alpha"));
        assert!(!parse_version("").present);
    }
}
