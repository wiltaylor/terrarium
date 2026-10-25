//! `vmlab.prepare` and `vmlab.release` (docs/host-protocol.md,
//! "`vmlab.prepare`").
//!
//! A lab instance is a directory `<instances_dir>/<instance>/`:
//!
//! - `src/`: the git worktree of a `repo` source, at the commit of `ref`.
//!   `git worktree add` makes it the first time; later prepares keep it.
//! - `lab/vmlab.wcl`: the lab file that vmlab runs, from the source
//!   definition after the allowlist check ([`crate::labcheck`]), with the
//!   attached workspace as an `@dev` decorator. vmlab keeps its state in
//!   `lab/.vmlab/`.
//!
//! The source of a `workspace` source is the agent workspace itself, so the
//! instance dir holds only `lab/`. Nothing is written into the workspace.
//!
//! These verbs run in the host agent as a task, not as one process: the
//! output goes to stderr, and the end is a `result` and an `exit`, or an
//! `error` frame.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::HostPaths;
use crate::labcheck;
use crate::protocol::{ErrorCode, LabSource, VmlabPrepare, VmlabRelease, WorkspaceMount};
use crate::vmlab::LAB_FILE;

/// The largest lab file the host agent reads.
const MAX_LAB_FILE: u64 = 1024 * 1024;
/// The default limit of each git or vmlab command.
const STEP_TIMEOUT: Duration = Duration::from_secs(300);

/// What the tasks need from the host agent.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub paths: HostPaths,
    pub git_bin: String,
    pub vmlab_bin: String,
}

/// A failed task: the code of the `error` frame and its message.
pub type TaskError = (ErrorCode, String);

fn bad(m: impl Into<String>) -> TaskError {
    (ErrorCode::BadRequest, m.into())
}

fn io(m: impl Into<String>) -> TaskError {
    (ErrorCode::Io, m.into())
}

/// An instance name: 1 to 128 of `[A-Za-z0-9._-]`, not starting with `.`
/// or `-`.
pub fn check_instance(name: &str) -> Result<(), TaskError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(bad(format!("bad instance name {name:?}")))
    }
}

/// A git ref: not empty, not starting with `-`, no `..`, and only
/// `[A-Za-z0-9._/+@-]`.
pub fn check_ref(r: &str) -> Result<(), TaskError> {
    let ok = !r.is_empty()
        && !r.starts_with('-')
        && !r.contains("..")
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '+' | '@' | '-'));
    if ok {
        Ok(())
    } else {
        Err(bad(format!("bad ref {r:?}")))
    }
}

/// A relative directory in a repo or a workspace: `""` and `.` are the
/// root; else plain segments with no `..`.
pub fn check_subdir(p: &str) -> Result<PathBuf, TaskError> {
    let p = p.trim_matches('/');
    if p.is_empty() || p == "." {
        return Ok(PathBuf::new());
    }
    for seg in p.split('/') {
        let ok = !seg.is_empty()
            && seg != ".."
            && !seg.starts_with('-')
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !ok {
            return Err(bad(format!("bad lab path {p:?}")));
        }
    }
    Ok(PathBuf::from(p))
}

/// The output of one command.
struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(String, String)],
    log: &mut Vec<String>,
) -> Result<Out, TaskError> {
    log.push(format!("$ {program} {}", args.join(" ")));
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = tokio::time::timeout(STEP_TIMEOUT, cmd.output())
        .await
        .map_err(|_| io(format!("{program} timed out")))?
        .map_err(|e| (ErrorCode::SpawnFailed, format!("cannot run {program}: {e}")))?;
    let o = Out {
        ok: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    for l in o.stdout.lines().chain(o.stderr.lines()) {
        log.push(format!("  {l}"));
    }
    Ok(o)
}

fn failed(what: &str, o: &Out) -> TaskError {
    io(format!(
        "{what} failed (exit {}): {}",
        o.code.map_or("-".into(), |c| c.to_string()),
        o.stderr.trim().lines().last().unwrap_or("").trim()
    ))
}

/// The git dir of a `repo` source, and the credential env of a URL.
async fn repo_dir(
    ctx: &Ctx,
    local: Option<&str>,
    url: Option<&str>,
    env: &std::collections::BTreeMap<String, String>,
    log: &mut Vec<String>,
) -> Result<PathBuf, TaskError> {
    match (local, url) {
        (Some(l), None) => {
            let rel = check_subdir(l)?;
            if rel.as_os_str().is_empty() {
                return Err(bad("an empty local repo name"));
            }
            let root = std::fs::canonicalize(&ctx.paths.repos_dir).map_err(|e| {
                (
                    ErrorCode::PathDenied,
                    format!(
                        "repos_dir {} is not usable ({e}); set repos_dir in the host agent config \
                         to the host path of the daemon's TERRARIUM_REPOS_DIR",
                        ctx.paths.repos_dir.display()
                    ),
                )
            })?;
            let real = std::fs::canonicalize(root.join(&rel)).map_err(|_| {
                (
                    ErrorCode::PathDenied,
                    format!("the repo {l} is not in repos_dir {}", root.display()),
                )
            })?;
            if !real.starts_with(&root) || real == root {
                return Err((
                    ErrorCode::PathDenied,
                    format!("the repo {l} leads out of repos_dir"),
                ));
            }
            Ok(real)
        }
        (None, Some(u)) => {
            if u.is_empty() || u.starts_with('-') || u.chars().any(char::is_control) {
                return Err(bad(format!("bad repo URL {u:?}")));
            }
            use sha2::Digest;
            let hash = crate::protocol::hex(&sha2::Sha256::digest(u.as_bytes()));
            let mirrors = ctx.paths.instances_dir.join(".mirrors");
            std::fs::create_dir_all(&mirrors)
                .map_err(|e| io(format!("cannot create {}: {e}", mirrors.display())))?;
            let dir = mirrors.join(format!("{}.git", &hash[..16]));
            let (cred_args, cred_env, _keyfile) = credential(env, &mirrors)?;
            let git = &ctx.git_bin;
            let d = dir.to_string_lossy().into_owned();
            let mut args: Vec<&str> = cred_args.iter().map(String::as_str).collect();
            let o = if dir.join("HEAD").is_file() {
                args.extend(["--git-dir", &d, "remote", "update", "--prune"]);
                run(git, &args, None, &cred_env, log).await?
            } else {
                args.extend(["clone", "--mirror", "--", u, &d]);
                run(git, &args, None, &cred_env, log).await?
            };
            if !o.ok {
                return Err(failed("the mirror fetch", &o));
            }
            Ok(dir)
        }
        _ => Err(bad("a repo source needs one of `local` and `url`")),
    }
}

/// The git `-c` args, the env, and the key file guard of a credential.
type Credential = (
    Vec<String>,
    Vec<(String, String)>,
    Option<tempfile::TempPath>,
);

/// The git `-c` args and env for the credential in `env`, as the checkout
/// on a target does: a helper that reads the token from the env, or an ssh
/// key in a 0600 file (removed when the guard drops).
fn credential(
    env: &std::collections::BTreeMap<String, String>,
    tmp_dir: &Path,
) -> Result<Credential, TaskError> {
    let mut args = Vec::new();
    let mut out = Vec::new();
    let mut guard = None;
    if let (Some(u), Some(p)) = (
        env.get("TERRARIUM_GIT_USERNAME"),
        env.get("TERRARIUM_GIT_PASSWORD"),
    ) {
        args.extend([
            "-c".to_owned(),
            "credential.helper=".to_owned(),
            "-c".to_owned(),
            "credential.helper=!f() { test \"$1\" = get || exit 0; echo \"username=$TERRARIUM_GIT_USERNAME\"; echo \"password=$TERRARIUM_GIT_PASSWORD\"; }; f".to_owned(),
        ]);
        out.push(("TERRARIUM_GIT_USERNAME".to_owned(), u.clone()));
        out.push(("TERRARIUM_GIT_PASSWORD".to_owned(), p.clone()));
    }
    if let Some(k) = env.get("TERRARIUM_GIT_SSH_KEY") {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = tempfile::NamedTempFile::new_in(tmp_dir)
            .map_err(|e| io(format!("cannot write the key file: {e}")))?;
        f.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io(format!("cannot set the key file mode: {e}")))?;
        writeln!(f, "{}", k.trim_end()).map_err(|e| io(format!("cannot write the key: {e}")))?;
        let path = f.into_temp_path();
        out.push((
            "GIT_SSH_COMMAND".to_owned(),
            format!(
                "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
                path.display()
            ),
        ));
        guard = Some(path);
    }
    Ok((args, out, guard))
}

/// Read the lab file of `dir`: a regular file (not a link), at most 1 MiB.
fn read_lab(dir: &Path) -> Result<String, TaskError> {
    let p = dir.join(LAB_FILE);
    let meta = std::fs::symlink_metadata(&p).map_err(|_| {
        bad(format!(
            "{} has no {LAB_FILE}; set lab_path to the directory that holds it",
            dir.display()
        ))
    })?;
    if !meta.file_type().is_file() {
        return Err((
            ErrorCode::Refused,
            format!("{} is not a regular file", p.display()),
        ));
    }
    if meta.len() > MAX_LAB_FILE {
        return Err((ErrorCode::Refused, format!("{} is too large", p.display())));
    }
    std::fs::read_to_string(&p).map_err(|e| io(format!("cannot read {}: {e}", p.display())))
}

/// Write `text` to `path` through a temp file and a rename, unless the
/// file holds it already (vmlab notices a changed lab file).
fn write_if_changed(path: &Path, text: &str) -> Result<bool, TaskError> {
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        return Ok(false);
    }
    let tmp = path.with_extension("wcl.tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| io(format!("cannot write {}: {e}", path.display())))?;
    Ok(true)
}

/// `vmlab.prepare`. Returns the `result` value. A prepare that fails
/// removes the instance dir (and its worktree) when it made it.
pub async fn prepare(
    ctx: &Ctx,
    req: &VmlabPrepare,
    log: &mut Vec<String>,
) -> Result<Value, TaskError> {
    check_instance(&req.instance)?;
    let made = !ctx.paths.instances_dir.join(&req.instance).exists();
    let res = prepare_in(ctx, req, log).await;
    if res.is_err() && made {
        let r = VmlabRelease {
            session: req.session,
            instance: req.instance.clone(),
            timeout_secs: None,
        };
        if let Err((_, e)) = release(ctx, &r, log).await {
            log.push(format!("cannot remove the new instance: {e}"));
        }
    }
    res
}

async fn prepare_in(
    ctx: &Ctx,
    req: &VmlabPrepare,
    log: &mut Vec<String>,
) -> Result<Value, TaskError> {
    if req.workspaces.len() > 1 {
        return Err(bad(
            "a vmlab machine syncs one workspace; vmlab.prepare takes at most one",
        ));
    }
    let inst = ctx.paths.instances_dir.join(&req.instance);
    std::fs::create_dir_all(&inst)
        .map_err(|e| io(format!("cannot create {}: {e}", inst.display())))?;
    let mut head = None;
    let (base, sub) = match &req.source {
        LabSource::Repo {
            local,
            url,
            git_ref,
            path,
        } => {
            check_ref(git_ref)?;
            let sub = check_subdir(path)?;
            let repo = repo_dir(ctx, local.as_deref(), url.as_deref(), &req.env, log).await?;
            let r = repo.to_string_lossy().into_owned();
            let git = &ctx.git_bin;
            let src = inst.join("src");
            if src.join(".git").is_file() {
                let o = run(
                    git,
                    &["-C", &src.to_string_lossy(), "rev-parse", "HEAD"],
                    None,
                    &[],
                    log,
                )
                .await?;
                head = Some(o.stdout.trim().to_owned());
                log.push(format!("the worktree {} is there; kept", src.display()));
            } else {
                if src.exists() {
                    return Err(io(format!(
                        "{} exists and is not a worktree; release the instance first",
                        src.display()
                    )));
                }
                let spec = format!("{git_ref}^{{commit}}");
                let o = run(
                    git,
                    &["--git-dir", &r, "rev-parse", "--verify", "--quiet", &spec],
                    None,
                    &[],
                    log,
                )
                .await?;
                let sha = o.stdout.trim().to_owned();
                if !o.ok || sha.len() < 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(bad(format!("the ref {git_ref} is not in the repo")));
                }
                let s = src.to_string_lossy().into_owned();
                let o = run(
                    git,
                    &["--git-dir", &r, "worktree", "add", "--detach", &s, &sha],
                    None,
                    &[],
                    log,
                )
                .await?;
                if !o.ok {
                    return Err(failed("git worktree add", &o));
                }
                head = Some(sha);
            }
            (src, sub)
        }
        LabSource::Workspace { workspace, path } => {
            let ws = crate::workspace::resolve(
                &ctx.paths.work_dir,
                &WorkspaceMount {
                    path: workspace.clone(),
                },
            )
            .map_err(|e| (e.code, e.message))?;
            (ws, check_subdir(path)?)
        }
    };
    let base_real = std::fs::canonicalize(&base)
        .map_err(|e| io(format!("cannot resolve {}: {e}", base.display())))?;
    let source = std::fs::canonicalize(base.join(&sub)).map_err(|_| {
        bad(format!(
            "{} has no directory {}",
            base.display(),
            sub.display()
        ))
    })?;
    if !source.starts_with(&base_real) {
        return Err((
            ErrorCode::PathDenied,
            format!("the lab path {} leads out of its source", sub.display()),
        ));
    }
    let text = read_lab(&source)?;
    let lab = labcheck::check(&text).map_err(|errs| {
        (
            ErrorCode::Refused,
            format!(
                "the lab definition {} is refused:\n- {}",
                source.join(LAB_FILE).display(),
                errs.join("\n- ")
            ),
        )
    })?;
    let attach = match req.workspaces.first() {
        None => None,
        Some(w) => {
            let machine = req.machine.clone().ok_or_else(|| {
                bad("a workspace attach needs `machine`: the machine that gets it")
            })?;
            let host = crate::workspace::resolve(&ctx.paths.work_dir, w)
                .map_err(|e| (e.code, e.message))?;
            Some(labcheck::Attach {
                machine,
                host,
                guest: w.target(),
            })
        }
    };
    let (out, dev) = labcheck::render(&text, &lab, &source, attach.as_ref())
        .map_err(|e| (ErrorCode::Refused, e))?;
    let lab_dir = inst.join("lab");
    std::fs::create_dir_all(&lab_dir)
        .map_err(|e| io(format!("cannot create {}: {e}", lab_dir.display())))?;
    let changed = write_if_changed(&lab_dir.join(LAB_FILE), &out)?;
    let o = run(&ctx.vmlab_bin, &["validate"], Some(&lab_dir), &[], log).await?;
    if !o.ok {
        return Err((
            ErrorCode::Refused,
            format!(
                "vmlab validate refused the lab: {}{}",
                o.stdout.trim(),
                o.stderr.trim()
            ),
        ));
    }
    let attached: Vec<&str> = req.workspaces.iter().map(|w| w.path.as_str()).collect();
    let mut v = json!({
        "lab": lab_dir,
        "instance": req.instance,
        "lab_name": lab.name,
        "source": source,
        "attached": attached,
        "dev": dev,
        "changed": changed,
    });
    if let Some(h) = head {
        v["head"] = h.into();
    }
    Ok(v)
}

/// `vmlab.release`. Returns the `result` value.
pub async fn release(
    ctx: &Ctx,
    req: &VmlabRelease,
    log: &mut Vec<String>,
) -> Result<Value, TaskError> {
    check_instance(&req.instance)?;
    let inst = ctx.paths.instances_dir.join(&req.instance);
    match std::fs::symlink_metadata(&inst) {
        Err(_) => return Ok(json!({"instance": req.instance, "released": false})),
        Ok(m) if !m.is_dir() => {
            return Err(io(format!("{} is not a directory", inst.display())));
        }
        Ok(_) => {}
    }
    let src = inst.join("src");
    let mut worktree = false;
    if src.join(".git").is_file() {
        let s = src.to_string_lossy().into_owned();
        let git = &ctx.git_bin;
        let o = run(
            git,
            &[
                "-C",
                &s,
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
            ],
            None,
            &[],
            log,
        )
        .await?;
        let common = o.stdout.trim().to_owned();
        if o.ok && !common.is_empty() {
            let o = run(
                git,
                &["--git-dir", &common, "worktree", "remove", "--force", &s],
                None,
                &[],
                log,
            )
            .await?;
            worktree = o.ok;
            let _ = run(
                git,
                &["--git-dir", &common, "worktree", "prune"],
                None,
                &[],
                log,
            )
            .await?;
        }
    }
    std::fs::remove_dir_all(&inst)
        .map_err(|e| io(format!("cannot remove {}: {e}", inst.display())))?;
    Ok(json!({"instance": req.instance, "released": true, "worktree_removed": worktree}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_refs_and_subdirs() {
        assert!(check_instance("vbox").is_ok());
        assert!(check_instance("vbox.01M5ABC").is_ok());
        for bad in ["", ".x", "-x", "a/b", "a b", "..", "x\n"] {
            assert!(check_instance(bad).is_err(), "{bad:?}");
        }
        for ok in ["main", "v1.2", "feature/x", "3f2c1e0b"] {
            assert!(check_ref(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "a..b", "a b", "x;y", "$(id)"] {
            assert!(check_ref(bad).is_err(), "{bad:?}");
        }
        assert_eq!(check_subdir("").unwrap(), PathBuf::new());
        assert_eq!(check_subdir(".").unwrap(), PathBuf::new());
        assert_eq!(
            check_subdir("alpine-box").unwrap(),
            PathBuf::from("alpine-box")
        );
        assert!(check_subdir("../x").is_err());
        assert!(check_subdir("a/../b").is_err());
    }

    fn git(dir: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    const LAB: &str = "import <vmlab.wcl>\nlab \"alpine-box\" {\n  container \"box\" {\n    image = \"alpine:3.22\"\n    profile = \"container\"\n    mode = :idle\n  }\n}\n";

    /// A repo `labs.git` with `alpine-box/vmlab.wcl` and `bad/vmlab.wcl`,
    /// a work dir with the workspace `p/a`, and a fake vmlab that logs.
    fn fixture() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let seed = root.join("seed");
        std::fs::create_dir_all(seed.join("alpine-box")).unwrap();
        std::fs::create_dir_all(seed.join("bad")).unwrap();
        std::fs::write(seed.join("alpine-box/vmlab.wcl"), LAB).unwrap();
        std::fs::write(
            seed.join("bad/vmlab.wcl"),
            LAB.replace(
                "mode = :idle",
                "mode = :idle\n    volume { host = \"/etc\" target = \"/x\" }",
            ),
        )
        .unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "labs"]);
        std::fs::create_dir_all(root.join("repos")).unwrap();
        git(root, &["clone", "-q", "--bare", "seed", "repos/labs.git"]);
        std::fs::create_dir_all(root.join("work/p/a")).unwrap();
        let vmlab = root.join("vmlab");
        std::fs::write(
            &vmlab,
            format!(
                "#!/bin/sh\necho \"vmlab $* (in $(pwd))\" >> '{}'\necho ok\n",
                root.join("vmlab.log").display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&vmlab, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ctx = Ctx {
            paths: HostPaths {
                work_dir: root.join("work"),
                instances_dir: root.join("instances"),
                repos_dir: root.join("repos"),
            },
            git_bin: "git".into(),
            vmlab_bin: vmlab.display().to_string(),
        };
        (dir, ctx)
    }

    fn req(path: &str, ws: Option<&str>) -> VmlabPrepare {
        VmlabPrepare {
            session: 1,
            instance: "vbox".into(),
            source: LabSource::Repo {
                local: Some("labs.git".into()),
                url: None,
                git_ref: "main".into(),
                path: path.into(),
            },
            machine: Some("box".into()),
            workspaces: ws
                .map(|p| vec![WorkspaceMount { path: p.into() }])
                .unwrap_or_default(),
            env: Default::default(),
            timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn prepare_adds_a_worktree_attaches_and_release_removes_it() {
        let (dir, ctx) = fixture();
        let mut log = Vec::new();
        let v = prepare(&ctx, &req("alpine-box", Some("p/a")), &mut log)
            .await
            .unwrap();
        let text = log.join("\n");
        assert!(text.contains("worktree add --detach"), "{text}");
        let lab = PathBuf::from(v["lab"].as_str().unwrap());
        assert_eq!(lab, ctx.paths.instances_dir.join("vbox/lab"));
        let out = std::fs::read_to_string(lab.join(LAB_FILE)).unwrap();
        let ws = std::fs::canonicalize(dir.path().join("work/p/a")).unwrap();
        assert!(
            out.contains(&format!(
                "@dev(workspace = \"{}\", workspace_guest = \"/work/p/a\")",
                ws.display()
            )),
            "{out}"
        );
        assert_eq!(v["attached"], json!(["p/a"]));
        assert_eq!(v["lab_name"], "alpine-box");
        assert_eq!(v["head"].as_str().unwrap().len(), 40);
        assert!(
            ctx.paths
                .instances_dir
                .join("vbox/src/alpine-box/vmlab.wcl")
                .is_file()
        );
        let calls = std::fs::read_to_string(dir.path().join("vmlab.log")).unwrap();
        assert!(
            calls.contains(&format!("vmlab validate (in {})", lab.display())),
            "{calls}"
        );
        // A second prepare keeps the worktree and the unchanged file.
        let v2 = prepare(&ctx, &req("alpine-box", Some("p/a")), &mut log)
            .await
            .unwrap();
        assert_eq!(v2["changed"], false);
        assert_eq!(v2["head"], v["head"]);
        let mut log = Vec::new();
        let r = release(
            &ctx,
            &VmlabRelease {
                session: 2,
                instance: "vbox".into(),
                timeout_secs: None,
            },
            &mut log,
        )
        .await
        .unwrap();
        assert_eq!(r["released"], true);
        assert_eq!(r["worktree_removed"], true, "{}", log.join("\n"));
        assert!(!ctx.paths.instances_dir.join("vbox").exists());
        let wt = std::process::Command::new("git")
            .args([
                "--git-dir",
                &dir.path().join("repos/labs.git").display().to_string(),
                "worktree",
                "list",
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&wt.stdout).lines().count(),
            1,
            "only the bare repo"
        );
    }

    #[tokio::test]
    async fn prepare_refuses_a_host_path_and_a_bad_source() {
        let (_dir, ctx) = fixture();
        let mut log = Vec::new();
        let (code, msg) = prepare(&ctx, &req("bad", None), &mut log)
            .await
            .unwrap_err();
        assert_eq!(code, ErrorCode::Refused);
        assert!(msg.contains("binds a host directory"), "{msg}");
        assert!(
            !ctx.paths.instances_dir.join("vbox").exists(),
            "a refused prepare leaves no instance and no worktree"
        );
        let (code, _) = prepare(&ctx, &req("../..", None), &mut log)
            .await
            .unwrap_err();
        assert_eq!(code, ErrorCode::BadRequest);
        let mut r = req("alpine-box", Some("p/nobody"));
        r.instance = "v2".into();
        let (code, msg) = prepare(&ctx, &r, &mut log).await.unwrap_err();
        assert_eq!(code, ErrorCode::PathDenied, "{msg}");
        let mut r = req("alpine-box", Some("p/a"));
        r.instance = "v3".into();
        r.machine = None;
        assert!(prepare(&ctx, &r, &mut log).await.is_err());
        let mut r = req("alpine-box", None);
        r.instance = "v4".into();
        r.source = LabSource::Repo {
            local: Some("../../etc".into()),
            url: None,
            git_ref: "main".into(),
            path: String::new(),
        };
        assert!(prepare(&ctx, &r, &mut log).await.is_err());
    }

    #[tokio::test]
    async fn a_workspace_source_writes_nothing_into_the_workspace() {
        let (dir, ctx) = fixture();
        let ws = dir.path().join("work/p/a");
        std::fs::write(ws.join("vmlab.wcl"), LAB).unwrap();
        let mut r = req("", Some("p/a"));
        r.instance = "vws.p.a".into();
        r.source = LabSource::Workspace {
            workspace: "p/a".into(),
            path: String::new(),
        };
        let mut log = Vec::new();
        let v = prepare(&ctx, &r, &mut log).await.unwrap();
        assert!(v.get("head").is_none());
        let entries: Vec<_> = std::fs::read_dir(&ws).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the lab file");
        assert!(
            ctx.paths
                .instances_dir
                .join("vws.p.a/lab/vmlab.wcl")
                .is_file()
        );
        assert!(!ctx.paths.instances_dir.join("vws.p.a/src").exists());
    }
}
