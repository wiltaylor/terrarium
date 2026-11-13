//! The managed checkout on a Windows host (TER-274 D8 to D13). It follows
//! `CHECKOUT_SCRIPT` step by step, with the trusted Git for Windows of the
//! host file, as the run-as account, inside one account workspace.
//!
//! Order of authority: the service audits the runtime, provisions the
//! workspace and makes the private credential directory. File operations in
//! the workspace run as the account (impersonation). Each git step is a new
//! process of the account in its own job object. Never spawn while
//! impersonating: the spawn impersonates by itself.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use terrarium_winapi::{Dir, Entry, Identity, PrivateDir, SpawnContext, TrustedFiles, Workspace};

use crate::git_checkout::{
    RUNTIME_FILES, TOKEN_HELPER, UNSET_ENV, check_request, is_managed, overrides, refused_key,
    result, ssh_command,
};
use crate::protocol::{CheckoutMode, GitCheckout, GitCredential};
use crate::windows_workspace::{locate, split_file};

/// A failed checkout: the exit code and a message for stderr.
#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    pub message: String,
}

fn fail(code: i32, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

fn hook_failure(stage: &str) -> Failure {
    fail(2, format!("checkout managed hook failed: {stage}"))
}

fn as_user<T>(
    identity: Option<&Arc<Identity>>,
    f: impl FnOnce() -> std::io::Result<T>,
) -> Result<T, Failure> {
    match identity {
        Some(identity) => identity.with_impersonation(f),
        None => f(),
    }
    .map_err(|e| fail(2, e.to_string()))
}

/// One git process at a time, as the account, with the checkout env.
struct Git<'a> {
    exe: std::path::PathBuf,
    env: BTreeMap<String, String>,
    overrides: Vec<String>,
    identity: Option<&'a Arc<Identity>>,
    cancel: &'a AtomicBool,
    deadline: Option<Instant>,
    stderr: &'a (dyn Fn(&[u8]) + Sync),
    credential: Option<(BTreeMap<String, String>, Vec<String>)>,
}

impl Git<'_> {
    /// `g()` of the script: with the credential when one is set.
    fn g(&self, args: &[&str]) -> Result<Vec<u8>, Failure> {
        let (env, extra) = self.credential.clone().unwrap_or_default();
        self.exec(&extra, args, &env)
    }

    /// `G()` of the script: no credential.
    fn plain(&self, args: &[&str]) -> Result<Vec<u8>, Failure> {
        self.exec(&[], args, &BTreeMap::new())
    }

    /// `G` that may fail: the exit code and stdout, never an error for a
    /// non-zero exit.
    fn status(&self, args: &[&str]) -> Result<(i32, Vec<u8>), Failure> {
        self.run(&[], args, &BTreeMap::new())
    }

    fn exec(
        &self,
        extra: &[String],
        args: &[&str],
        env: &BTreeMap<String, String>,
    ) -> Result<Vec<u8>, Failure> {
        let (code, out) = self.run(extra, args, env)?;
        if code != 0 {
            return Err(fail(code, format!("git {} failed", args.first().unwrap_or(&""))));
        }
        Ok(out)
    }

    /// "forget" of the script: drop the credential env and helper.
    fn forget(&mut self) {
        self.credential = None;
    }

    fn run(
        &self,
        extra: &[String],
        args: &[&str],
        env: &BTreeMap<String, String>,
    ) -> Result<(i32, Vec<u8>), Failure> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(fail(130, "checkout cancelled"));
        }
        let mut argv: Vec<String> = self.overrides.clone();
        argv.extend(extra.iter().cloned());
        argv.extend(args.iter().map(|s| (*s).to_owned()));
        let mut child_env = self.env.clone();
        child_env.extend(env.clone());
        let child = terrarium_winapi::spawn_with_context(
            &self.exe,
            &argv,
            None,
            &child_env,
            None,
            terrarium_winapi::Limits::default(),
            SpawnContext {
                holder: None,
                identity: self.identity,
                remove_env: &UNSET_ENV,
            },
        )
        .map_err(|e| fail(2, format!("cannot start git: {e}")))?;
        drop(child_env);
        let terrarium_winapi::Child {
            job,
            process,
            console: _,
            stdin,
            mut stdout,
            stderr,
        } = child;
        drop(stdin);
        let out = std::thread::spawn(move || {
            let mut data = Vec::new();
            let _ = (&mut stdout).take(1 << 20).read_to_end(&mut data);
            data
        });
        let code = std::thread::scope(|scope| {
            if let Some(mut stderr) = stderr {
                let forward = self.stderr;
                scope.spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = stderr.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        forward(&buf[..n]);
                    }
                });
            }
            let code = loop {
                match process.try_wait() {
                    Ok(Some(code)) => break Ok(code as i32),
                    Ok(None) => {}
                    Err(e) => break Err(fail(2, format!("git wait: {e}"))),
                }
                if self.cancel.load(Ordering::SeqCst)
                    || self.deadline.is_some_and(|d| Instant::now() >= d)
                {
                    break Err(fail(124, "checkout cancelled or timed out"));
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            // End any descendant (ssh, sh) still holding the pipes, so the
            // stderr reader of this scope can finish.
            let _ = job.terminate();
            code
        });
        // Wait for the job to drain before the caller may remove the
        // credential files (D20).
        let drained = job.wait_empty(Duration::from_secs(10));
        drop(job);
        let out = out.join().unwrap_or_default();
        if !drained {
            return Err(fail(2, "git processes did not end: cleanup is unconfirmed"));
        }
        code.map(|c| (c, out))
    }
}

fn text(out: Vec<u8>) -> String {
    String::from_utf8_lossy(&out).trim().to_owned()
}

/// Run a checkout. `stderr` receives git's error output as it comes.
pub fn run(
    req: &GitCheckout,
    identity: Option<&Arc<Identity>>,
    runtime_root: &Path,
    cancel: &AtomicBool,
    stderr: &(dyn Fn(&[u8]) + Sync),
) -> Result<serde_json::Value, Failure> {
    check_request(req).map_err(|m| fail(2, m))?;
    let dest = req
        .dest
        .to_str()
        .ok_or_else(|| fail(2, "dest is not a Windows workspace path"))?;
    let location = match locate(dest) {
        Some(Ok(l)) => l,
        Some(Err(m)) => return Err(fail(2, m)),
        None => return Err(fail(2, format!("{dest} is not in a workspace"))),
    };
    let (dirs, name) = split_file(&location).map_err(|m| fail(2, m))?;
    // Service authority: runtime audit, workspace, private directory.
    let runtime = TrustedFiles::open(runtime_root, &RUNTIME_FILES)
        .map_err(|e| fail(2, format!("trusted Git for Windows: {e}")))?;
    let root = runtime.root().to_owned();
    let workspace = Workspace::open(
        &location.project,
        &location.agent,
        identity.map(Arc::as_ref),
        true,
    )
    .map_err(|e| fail(2, format!("workspace: {e}")))?;
    let mut private = PrivateDir::create(u64::from(req.session), identity.map(Arc::as_ref))
        .map_err(|e| fail(2, format!("git session directory: {e}")))?;
    let hooks_off = private.subdir("hooks").map_err(|e| fail(2, e.to_string()))?;
    let mut known = req.known_hosts.clone().unwrap_or_default();
    known.push('\n');
    let known_hosts = private
        .secret_file("known_hosts", known.as_bytes())
        .map_err(|e| fail(2, e.to_string()))?;
    let mut key_file = match &req.credential {
        Some(GitCredential::SshKey { key }) => {
            let mut key = key.trim_end().to_owned();
            key.push('\n');
            Some(
                private
                    .secret_file("key", key.as_bytes())
                    .map_err(|e| fail(2, format!("cannot write the SSH key: {e}")))?,
            )
        }
        _ => None,
    };
    let path_of = |p: &Path| p.to_str().map(str::to_owned).ok_or_else(|| fail(2, "non-ASCII path"));
    let ssh = path_of(&root.join(r"usr\bin\ssh.exe"))?;
    let known_path = path_of(known_hosts.path())?;
    let ssh_base = ssh_command(&ssh, &known_path, None).map_err(|m| fail(2, m))?;
    let false_exe = path_of(&root.join(r"usr\bin\false.exe"))?;
    let root_text = path_of(&root)?;
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".into(),
        format!(r"{root_text}\cmd;{root_text}\mingw64\bin;{root_text}\usr\bin;C:\Windows\System32"),
    );
    for (k, v) in [
        ("GIT_CONFIG_GLOBAL", "NUL"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LC_ALL", "C"),
    ] {
        env.insert(k.into(), v.into());
    }
    env.insert("GIT_ASKPASS".into(), false_exe.clone());
    env.insert("SSH_ASKPASS".into(), false_exe);
    env.insert("GIT_SSH_COMMAND".into(), ssh_base.clone());
    let credential = match &req.credential {
        Some(GitCredential::Token { username, password }) => Some((
            BTreeMap::from([
                ("TERRARIUM_GIT_USERNAME".to_owned(), username.clone()),
                ("TERRARIUM_GIT_PASSWORD".to_owned(), password.clone()),
            ]),
            vec!["-c".to_owned(), format!("credential.helper={TOKEN_HELPER}")],
        )),
        Some(GitCredential::SshKey { .. }) => {
            let key = path_of(key_file.as_ref().expect("key file").path())?;
            let cmd = ssh_command(&ssh, &known_path, Some(&key)).map_err(|m| fail(2, m))?;
            Some((BTreeMap::from([("GIT_SSH_COMMAND".to_owned(), cmd)]), Vec::new()))
        }
        None => None,
    };
    let hooks_text = path_of(&hooks_off)?;
    let parent = as_user(identity, || workspace.into_dir(&dirs, true))?;
    let existing = as_user(identity, || parent.entry(name))?;
    let existing_repo = match existing {
        Some(Entry::File) => return Err(fail(2, format!("{dest} is a file"))),
        Some(Entry::Dir) => {
            let dest_dir = as_user(identity, || parent.sub(&[name], false))?;
            match as_user(identity, || dest_dir.entry(".git"))? {
                Some(Entry::Dir) => true,
                Some(Entry::File) => return Err(hook_failure("mode")),
                None => false,
            }
        }
        None => false,
    };
    let safe = existing_repo.then(|| dest.replace('\\', "/"));
    let mut git = Git {
        exe: root.join(r"cmd\git.exe"),
        env,
        overrides: overrides(&hooks_text, safe.as_deref()),
        identity,
        cancel,
        deadline: req
            .timeout_secs
            .filter(|t| *t > 0)
            .map(|t| Instant::now() + Duration::from_secs(t)),
        stderr,
        credential,
    };
    let (action, note) = if existing_repo {
        let git_dir = as_user(identity, || parent.sub(&[name, ".git"], false))?;
        let bad = if as_user(identity, || git_dir.entry("commondir"))?.is_some() {
            Some(".git/commondir".to_owned())
        } else {
            let config = format!(r"{dest}\.git\config");
            match git.status(&["config", "--file", &config, "--name-only", "--list"])? {
                (0, out) => {
                    let names = String::from_utf8_lossy(&out).into_owned();
                    refused_key(names.lines()).map(str::to_owned)
                }
                _ => Some(".git/config".to_owned()),
            }
        };
        drop(git_dir);
        if let Some(bad) = bad {
            git.forget();
            drop(key_file.take());
            (
                "not_updated",
                format!("the repo config has `{bad}`, which the checkout does not allow; remove it"),
            )
        } else {
            let before = text(git.status(&["-C", dest, "rev-parse", "-q", "--verify", "HEAD"])?.1);
            let reference = match &req.branch {
                Some(b) => b.clone(),
                None => match git.status(&["-C", dest, "symbolic-ref", "--short", "-q", "HEAD"])? {
                    (0, out) => text(out),
                    _ => "HEAD".into(),
                },
            };
            let depth = req.depth.map(|d| d.to_string());
            let mut fetch = vec!["-C", dest, "fetch"];
            if let Some(d) = &depth {
                fetch.extend(["--depth", d.as_str()]);
            }
            fetch.extend(["--", req.url.as_str(), reference.as_str()]);
            git.g(&fetch)?;
            git.forget();
            drop(key_file.take());
            if !git.plain(&["-C", dest, "status", "--porcelain"])?.is_empty() {
                ("not_updated", "the work tree has local changes".to_owned())
            } else if git
                .status(&["-C", dest, "merge", "--ff-only", "-q", "FETCH_HEAD"])?
                .0
                == 0
            {
                let after =
                    text(git.status(&["-C", dest, "rev-parse", "-q", "--verify", "HEAD"])?.1);
                (
                    if before == after { "up_to_date" } else { "updated" },
                    String::new(),
                )
            } else {
                ("not_updated", "the branch has diverged".to_owned())
            }
        }
    } else {
        let depth = req.depth.map(|d| d.to_string());
        let mut clone = vec!["clone"];
        if let Some(d) = &depth {
            clone.extend(["--depth", d.as_str()]);
        }
        if let Some(b) = &req.branch {
            clone.extend(["--branch", b.as_str()]);
        }
        clone.extend(["--", req.url.as_str(), dest]);
        git.g(&clone)?;
        git.forget();
        drop(key_file.take());
        ("cloned", String::new())
    };
    drop(known_hosts);
    if let CheckoutMode::Managed(managed) = &req.mode
        && action != "not_updated"
    {
        install_hooks(&parent, name, managed, identity, u64::from(req.session))?;
        for (key, value) in [
            ("user.name", managed.name.clone()),
            ("user.email", managed.email.clone()),
            ("terrarium.provenance", managed.provenance.to_string()),
            ("terrarium.provenanceWarnOnly", managed.warn_only.to_string()),
            ("terrarium.repoId", managed.repo_id.clone()),
        ] {
            git.plain(&["-C", dest, "config", key, &value])?;
        }
    }
    let head = text(git.status(&["-C", dest, "rev-parse", "-q", "--verify", "HEAD"])?.1);
    let branch = match git.status(&["-C", dest, "symbolic-ref", "--short", "-q", "HEAD"])? {
        (0, out) => text(out),
        _ => String::new(),
    };
    drop(private);
    drop(runtime);
    Ok(result(action, &head, &branch, dest, &note))
}

/// The hook install of the script, through the no-reparse walk. All checks
/// come before any change. A repo hook that is not ours is kept as `.repo`.
fn install_hooks(
    parent: &Dir,
    name: &str,
    managed: &crate::protocol::ManagedHooks,
    identity: Option<&Arc<Identity>>,
    session: u64,
) -> Result<(), Failure> {
    let install = || -> std::io::Result<Result<(), &'static str>> {
        let git_dir = parent.sub(&[name, ".git"], false)?;
        let hooks = git_dir.sub(&["hooks"], true)?;
        let plan = [
            ("prepare-commit-msg", managed.commit_hook.as_str()),
            ("pre-push", managed.push_hook.as_str()),
        ];
        let mut keep = Vec::new();
        for (hook, _) in plan {
            let backup = format!("{hook}.repo");
            let current = hooks.entry(hook)?;
            let saved = hooks.entry(&backup)?;
            if current == Some(Entry::Dir) || saved == Some(Entry::Dir) {
                return Ok(Err("install"));
            }
            if current == Some(Entry::File) {
                let mut content = String::new();
                hooks.open_read(hook)?.take(1 << 20).read_to_string(&mut content)?;
                if !is_managed(hook, &content) {
                    if saved.is_some() {
                        return Ok(Err("install"));
                    }
                    keep.push((hook, backup));
                }
            }
        }
        let mut staged = Vec::new();
        for (hook, content) in plan {
            let mut temp = hooks.create_temp(session)?;
            temp.file().write_all(content.as_bytes())?;
            staged.push((hook, temp));
        }
        for (hook, backup) in keep {
            hooks.rename_no_replace(hook, &backup)?;
        }
        for (hook, temp) in staged {
            temp.publish(&hooks, hook)?;
        }
        Ok(Ok(()))
    };
    match identity {
        Some(identity) => identity.with_impersonation(install),
        None => install(),
    }
    .map_err(|_| hook_failure("install"))?
    .map_err(hook_failure)
}
