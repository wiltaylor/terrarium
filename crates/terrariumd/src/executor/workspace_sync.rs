//! Opt-in SSH workspace mirroring. Never follow workspace symlinks.
use super::*;
use std::collections::BTreeSet;
use std::sync::{OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const EXCLUDES: &[&str] = &[
    ".git",
    ".terrarium",
    "target",
    "node_modules",
    ".terrarium-stage",
    ".terrarium-git-trailers",
    ".env",
    ".env.*",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "id_rsa",
    "id_rsa.pub",
    "id_ed25519",
    "id_ed25519.pub",
    "id_ed25519_sk",
    "id_ed25519_sk.pub",
    "id_ecdsa",
    "id_ecdsa.pub",
    "id_ecdsa_sk",
    "id_ecdsa_sk.pub",
    "id_dsa",
    "id_dsa.pub",
    "*.pem",
    ".ssh",
];
type Snapshot = BTreeMap<PathBuf, Vec<u8>>;

fn excluded(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let name = name.as_bytes();
    EXCLUDES.iter().any(|pattern| {
        let pattern = pattern.as_bytes();
        if let Some(prefix) = pattern.strip_suffix(b"*") {
            name.starts_with(prefix)
        } else if let Some(suffix) = pattern.strip_prefix(b"*") {
            name.ends_with(suffix)
        } else {
            name == pattern
        }
    })
}

fn changed_paths(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    paths
        .into_iter()
        .filter(|p| before.get(*p) != after.get(*p))
        .map(|p| p.display().to_string())
        .collect()
}

fn conflict(changed: Vec<String>) -> ApiError {
    ApiError::invalid(format!(
        "sync_conflict: local edits in {}; remote output retained on target",
        changed.join(", ")
    ))
}

fn rsync_reason(code: Option<i32>) -> &'static str {
    match code {
        Some(1) => "invalid rsync options",
        Some(2) => "protocol mismatch",
        Some(3) => "file selection failed",
        Some(5) => "remote protocol startup failed",
        Some(10) => "socket I/O failed",
        Some(11) => "file I/O failed",
        Some(12) => "protocol stream failed",
        Some(23) => "partial transfer",
        Some(24) => "source files vanished",
        Some(30 | 35) => "connection timed out",
        Some(255) => "SSH connection failed (check access and remote rsync)",
        None => "terminated by signal",
        _ => "transfer failed",
    }
}

fn snapshot(root: &Path) -> std::io::Result<Snapshot> {
    fn visit(root: &Path, dir: &Path, out: &mut Snapshot) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if excluded(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            let mut hash = Sha256::new();
            hash.update(meta.permissions().mode().to_le_bytes());
            if meta.is_symlink() {
                use std::os::unix::ffi::OsStrExt;
                hash.update(b"link");
                hash.update(std::fs::read_link(&path)?.as_os_str().as_bytes());
            } else if meta.is_dir() {
                hash.update(b"dir");
                visit(root, &path, out)?;
            } else if meta.is_file() {
                use std::io::Read;
                let mut file = std::fs::File::open(&path)?;
                let mut buffer = [0u8; 65536];
                loop {
                    let n = file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    hash.update(&buffer[..n]);
                }
            } else {
                continue;
            }
            out.insert(
                path.strip_prefix(root)
                    .map_err(std::io::Error::other)?
                    .to_owned(),
                hash.finalize().to_vec(),
            );
        }
        Ok(())
    }
    let mut out = Snapshot::new();
    visit(root, root, &mut out)?;
    Ok(out)
}

async fn scan(root: &Path) -> ApiResult<Snapshot> {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || snapshot(&root))
        .await
        .map_err(|e| ApiError::invalid(format!("workspace scan: {e}")))?
        .map_err(|e| ApiError::invalid(format!("workspace scan: {e}")))
}

fn workspace_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, v| v.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(path.to_owned(), Arc::downgrade(&lock));
    lock
}

pub(super) struct Sync {
    root: PathBuf,
    remote: String,
    shell: String,
    before: Snapshot,
    _key: Option<crate::secrets::RunDir>,
    _lock: OwnedMutexGuard<()>,
}

async fn rsync(source: &str, destination: &str, shell: Option<&str>) -> ApiResult<()> {
    rsync_basis(source, destination, shell, None, false)
        .await
        .map(|_| ())
}

async fn rsync_basis(
    source: &str,
    destination: &str,
    shell: Option<&str>,
    basis: Option<&Path>,
    skip_links: bool,
) -> ApiResult<String> {
    let mut cmd = tokio::process::Command::new("rsync");
    cmd.args([
        "-a",
        "--no-owner",
        "--no-group",
        "--chmod=ugo-s",
        "--no-specials",
        "--no-devices",
        "--delete",
        "--checksum",
        "--protect-args",
        "--stats",
    ]);
    // Delta encoding is useful across SSH, not for the local copies.
    if shell.is_some() {
        cmd.arg("--no-whole-file");
    }
    if skip_links {
        cmd.arg("--no-links");
    }
    if let Some(basis) = basis {
        cmd.arg("--copy-dest").arg(basis);
    }
    for name in EXCLUDES {
        cmd.arg(format!("--exclude={name}"));
    }
    if let Some(shell) = shell {
        cmd.args(["-e", shell]);
    }
    cmd.args(["--", source, destination]);
    cmd.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(300), cmd.output())
        .await
        .map_err(|_| ApiError::invalid("workspace rsync timed out"))?
        .map_err(|e| {
            ApiError::invalid(format!(
                "workspace rsync (install rsync on daemon and target): {e}"
            ))
        })?;
    if !output.status.success() {
        // Do not expose stderr: filenames or remote diagnostics may contain secrets.
        return Err(ApiError::invalid(format!(
            "workspace rsync failed ({}): {}",
            output.status,
            rsync_reason(output.status.code())
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) async fn prepare(
    state: &Shared,
    actor: &Actor,
    def: &TargetDef,
    selected: Option<&str>,
) -> ApiResult<Option<Sync>> {
    if !def.sync_workspace {
        return Ok(None);
    }
    // An exedev target comes here as the ssh target of its VM
    // (`executor::resolved_def`).
    if def.kind != TargetKind::Ssh {
        return Err(ApiError::invalid("sync_workspace requires an ssh target"));
    }
    let holder = holder_of(actor);
    let id = holder
        .as_deref()
        .or(selected)
        .ok_or_else(|| ApiError::invalid("workspace sync requires an agent (use --agent)"))?;
    let info = holder_info(state, id, actor.agent().map(|a| a.project.as_str())).await?;
    let root = state.work_dir.join(
        info.rel
            .ok_or_else(|| ApiError::invalid("agent has no workspace"))?,
    );
    let lock = workspace_lock(&root)
        .try_lock_owned()
        .map_err(|_| ApiError::invalid("workspace sync already in progress for this agent"))?;
    let key = if let Some(name) = &def.credential {
        match crate::secrets::load_now(state, crate::store::targets::owner(def), name)? {
            Secret::SshKey(k) => {
                let dir = RunDirs::new(&state.state_dir).run_dir()?;
                dir.write_private("id", k.as_bytes())?;
                Some(dir)
            }
            _ => {
                return Err(ApiError::invalid(
                    "workspace sync needs an ssh_key credential",
                ));
            }
        }
    } else {
        None
    };
    // Reuse the executor's quoting, identity, port and host-key policy.
    let key_path = key.as_ref().map(|dir| dir.path().join("id"));
    let plan = ssh_plan(
        &state.executor.ssh_bin,
        def,
        key_path.as_deref(),
        &known_hosts(&state.state_dir)?,
        &RemoteCmd {
            argv: vec!["true".into()],
            ..RemoteCmd::default()
        },
    )?;
    let shell = std::iter::once(plan.program.as_str())
        .chain(plan.args[..plan.args.len() - 3].iter().map(String::as_str))
        .map(sh_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let mkdir = start_cmd(
        state,
        def,
        &RemoteCmd {
            argv: vec![
                "mkdir".into(),
                "-p".into(),
                "--".into(),
                root.display().to_string(),
            ],
            timeout_secs: Some(30),
            ..RemoteCmd::default()
        },
    )?
    .collect(4096)
    .await;
    if !mkdir.end().success() {
        return Err(ApiError::invalid("could not create remote workspace"));
    }
    let before = scan(&root).await?;
    let address = def
        .address
        .as_deref()
        .ok_or_else(|| ApiError::invalid("an ssh target needs address"))?;
    let remote = format!("{address}:{}/", root.display());
    rsync(&format!("{}/", root.display()), &remote, Some(&shell)).await?;
    Ok(Some(Sync {
        root,
        remote,
        shell,
        before,
        _key: key,
        _lock: lock,
    }))
}

impl Sync {
    async fn download(&self, stage: &Path) -> ApiResult<String> {
        let basis =
            tempfile::tempdir().map_err(|e| ApiError::invalid(format!("sync basis: {e}")))?;
        // rsync must never resolve live-workspace symlinks as basis paths.
        // Seed a private, symlink-free copy. No other process can replace its
        // directories during the remote transfer, and no hard links are used.
        rsync_basis(
            &format!("{}/", self.root.display()),
            &format!("{}/", basis.path().display()),
            None,
            None,
            true,
        )
        .await?;
        rsync_basis(
            &self.remote,
            &format!("{}/", stage.display()),
            Some(&self.shell),
            Some(basis.path()),
            false,
        )
        .await
    }

    async fn pull(&self) -> ApiResult<()> {
        let stage =
            tempfile::tempdir().map_err(|e| ApiError::invalid(format!("sync staging: {e}")))?;
        self.download(stage.path()).await?;
        let now = scan(&self.root).await?;
        let changed = changed_paths(&self.before, &now);
        if !changed.is_empty() {
            return Err(conflict(changed));
        }
        let expected = scan(stage.path()).await?;
        rsync(
            &format!("{}/", stage.path().display()),
            &format!("{}/", self.root.display()),
            None,
        )
        .await?;
        let changed = changed_paths(&expected, &scan(&self.root).await?);
        if !changed.is_empty() {
            return Err(conflict(changed));
        }
        Ok(())
    }
}

pub(super) fn finish_with(sess: &Arc<ExecSession>, sync: Option<Sync>) {
    let Some(sync) = sync else { return };
    let (tx, mut rx) = mpsc::unbounded_channel();
    *sess.sync_end.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let sess = Arc::downgrade(sess);
    tokio::spawn(async move {
        if let Some(end) = rx.recv().await {
            // A killed/lost process may still be writing. Leave its output remote.
            let mut end = if matches!(
                end,
                End::Cp { .. }
                    | End::Exit {
                        code: Some(_),
                        signal: None,
                        ..
                    }
            ) {
                match sync.pull().await {
                    Ok(()) => end,
                    Err(e) => End::error(
                        if e.message.starts_with("sync_conflict:") {
                            "sync_conflict"
                        } else {
                            "sync_failed"
                        },
                        e.message,
                    ),
                }
            } else {
                end
            };
            if let Some(sess) = sess.upgrade() {
                // Serialize handoff with finish(): events received during the pull
                // must not hide a pull error that left output remote.
                let mut sender = sess.sync_end.lock().unwrap_or_else(|e| e.into_inner());
                sender.take();
                while let Ok(pending) = rx.try_recv() {
                    end = match end {
                        End::Error { code, message }
                            if code == "sync_conflict" || code == "sync_failed" =>
                        {
                            End::error(
                                &code,
                                format!(
                                    "{message}; pending session end: {}",
                                    serde_json::to_string(&pending).unwrap_or_default()
                                ),
                            )
                        }
                        _ => pending,
                    };
                }
                sess.finish_synced(end);
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const SECRET_NAMES: &[&str] = &[
        ".env",
        ".env.production",
        ".netrc",
        ".npmrc",
        ".pypirc",
        "id_ed25519",
        "id_rsa.pub",
        "id_ed25519_sk",
        "id_ed25519_sk.pub",
        "id_ecdsa_sk",
        "id_ecdsa_sk.pub",
        "certificate.pem",
        ".terrarium-git-trailers",
    ];

    #[test]
    fn secret_patterns_match_real_names_at_every_depth() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        for name in SECRET_NAMES {
            assert!(excluded(std::ffi::OsStr::new(name)));
            std::fs::write(root.path().join("nested").join(name), "secret").unwrap();
        }
        std::fs::create_dir(root.path().join("nested/.ssh")).unwrap();
        std::fs::write(root.path().join("nested/.ssh/config"), "secret").unwrap();
        assert_eq!(snapshot(root.path()).unwrap().len(), 1);
        assert!(!excluded(std::ffi::OsStr::new("environment.rs")));
        assert!(!excluded(std::ffi::OsStr::new("id_parser.rs")));
        assert_eq!(rsync_reason(Some(23)), "partial transfer");
        assert_eq!(
            rsync_reason(Some(255)),
            "SSH connection failed (check access and remote rsync)"
        );
    }

    #[test]
    fn sync_is_opt_in_and_ssh_only() {
        let mut def: TargetDef = serde_json::from_value(json!({
            "name": "fixture", "kind": "ssh", "address": "localhost"
        }))
        .unwrap();
        assert!(!def.sync_workspace);
        assert!(
            serde_json::to_value(&def)
                .unwrap()
                .get("sync_workspace")
                .is_none()
        );
        def.sync_workspace = true;
        assert!(crate::store::targets::validate(&def).is_ok());
        def.kind = TargetKind::Host;
        assert!(
            crate::store::targets::validate(&def)
                .unwrap_err()
                .message
                .contains("requires an ssh target")
        );
    }

    #[test]
    fn snapshot_tracks_content_permissions_links_and_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("file"), "one").unwrap();
        symlink("file", root.join("link")).unwrap();
        let first = snapshot(root).unwrap();
        std::fs::write(root.join("file"), "two").unwrap();
        assert_ne!(first, snapshot(root).unwrap());
        std::fs::write(root.join("file"), "one").unwrap();
        std::fs::set_permissions(root.join("file"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert_ne!(first, snapshot(root).unwrap());
        std::fs::remove_file(root.join("link")).unwrap();
        assert!(!snapshot(root).unwrap().contains_key(Path::new("link")));
    }

    #[test]
    fn snapshot_excludes_caches_and_does_not_follow_links() {
        let root = tempfile::tempdir().unwrap();
        for name in EXCLUDES {
            std::fs::create_dir(root.path().join(name)).unwrap();
            std::fs::write(root.path().join(name).join("secret"), "ignored").unwrap();
        }
        symlink("/does/not/exist", root.path().join("link")).unwrap();
        assert_eq!(snapshot(root.path()).unwrap().len(), 1);
    }

    #[test]
    fn workspace_operations_are_serialized() {
        let path = Path::new("/fixture/workspace");
        let held = workspace_lock(path).try_lock_owned().unwrap();
        assert!(workspace_lock(path).try_lock_owned().is_err());
        drop(held);
        assert!(workspace_lock(path).try_lock_owned().is_ok());
    }

    #[tokio::test]
    async fn session_waits_for_pull_and_reports_conflicts() {
        if std::process::Command::new("rsync")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("result"), "remote").unwrap();
        let sync = Sync {
            root: local.path().to_owned(),
            remote: format!("{}/", remote.path().display()),
            shell: "ssh".into(),
            before: snapshot(local.path()).unwrap(),
            _key: None,
            _lock: workspace_lock(local.path()).try_lock_owned().unwrap(),
        };
        let (session, _ctl) = ExecSession::new("fixture", None, "exec", 1024);
        finish_with(&session, Some(sync));
        let end = End::Exit {
            code: Some(7),
            signal: None,
            reason: "exited".into(),
        };
        session.finish(end.clone());
        assert!(!session.is_done());
        tokio::time::timeout(Duration::from_secs(10), async {
            while !session.is_done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(session.end(), Some(end));
        assert_eq!(
            std::fs::read_to_string(local.path().join("result")).unwrap(),
            "remote"
        );

        let sync = Sync {
            root: local.path().to_owned(),
            remote: format!("{}/", remote.path().display()),
            shell: "ssh".into(),
            before: snapshot(local.path()).unwrap(),
            _key: None,
            _lock: workspace_lock(local.path()).try_lock_owned().unwrap(),
        };
        let (revoked, _ctl) = ExecSession::new("revoked", None, "exec", 1024);
        finish_with(&revoked, Some(sync));
        revoked.finish(End::Exit {
            code: Some(0),
            signal: None,
            reason: "exited".into(),
        });
        // Let the receiver start its asynchronous pull before revoking.
        tokio::task::yield_now().await;
        let end = End::error("lease_revoked", "lease revoked");
        revoked.finish(end.clone());
        tokio::time::timeout(Duration::from_secs(10), async {
            while !revoked.is_done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(revoked.end(), Some(end));

        let sync = Sync {
            root: local.path().to_owned(),
            remote: format!("{}/", remote.path().display()),
            shell: "ssh".into(),
            before: snapshot(local.path()).unwrap(),
            _key: None,
            _lock: workspace_lock(local.path()).try_lock_owned().unwrap(),
        };
        std::fs::write(local.path().join("result"), "harness").unwrap();
        let (session, _ctl) = ExecSession::new("fixture", None, "push", 1024);
        finish_with(&session, Some(sync));
        session.finish(End::Cp {
            bytes: 0,
            sha256: String::new(),
        });
        tokio::task::yield_now().await;
        session.finish(End::error("lease_revoked", "lease revoked"));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !session.is_done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(session.end(), Some(End::Error { code, message }) if code == "sync_conflict" && message.contains("lease_revoked") && message.contains("remote output retained"))
        );
        assert_eq!(
            std::fs::read_to_string(local.path().join("result")).unwrap(),
            "harness"
        );
    }

    // A private local sshd, not an external VM. Enable with the sshd binary
    // path on test hosts that have openssh-server and rsync installed.
    #[tokio::test]
    async fn ssh_roundtrip_deletes_large_tree_and_conflict() {
        let Some(sshd) = std::env::var_os("TERRARIUM_TEST_SSHD") else {
            return;
        };
        let fixture = tempfile::tempdir().unwrap();
        let local = fixture.path().join("local");
        let remote = fixture.path().join("remote");
        std::fs::create_dir(&local).unwrap();
        std::fs::create_dir(&remote).unwrap();
        let key = fixture.path().join("key");
        assert!(
            std::process::Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&key)
                .status()
                .unwrap()
                .success()
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let user = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap();
        let user = String::from_utf8(user.stdout).unwrap().trim().to_owned();
        let config = fixture.path().join("sshd_config");
        std::fs::write(&config, format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin yes\nPidFile {}/pid\n", key.display(), key.display(), fixture.path().display())).unwrap();
        let mut server = tokio::process::Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(config)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let shell = format!(
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={} -o IdentitiesOnly=yes -i {} -p {port}",
            sh_quote(&fixture.path().join("known_hosts").display().to_string()),
            sh_quote(&key.display().to_string())
        );
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            assert!(server.try_wait().unwrap().is_none(), "fixture sshd exited");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for i in 0..2000 {
            std::fs::write(local.join(format!("source-{i}")), format!("content-{i}")).unwrap();
        }
        std::fs::write(
            local.join("hello.c"),
            "#include <stdio.h>\nint main(void) { fputs(\"build output\", stdout); return 0; }\n",
        )
        .unwrap();
        std::fs::create_dir(local.join("target")).unwrap();
        std::fs::write(local.join("target/cache"), "local cache").unwrap();
        for name in SECRET_NAMES {
            std::fs::write(local.join(name), "local secret").unwrap();
            std::fs::write(remote.join(name), "remote secret").unwrap();
        }
        std::fs::create_dir(local.join(".ssh")).unwrap();
        std::fs::write(local.join(".ssh/config"), "local secret").unwrap();
        let remote_spec = format!("{user}@127.0.0.1:{}/", remote.display());
        rsync(&format!("{}/", local.display()), &remote_spec, Some(&shell))
            .await
            .unwrap();
        assert_eq!(snapshot(&local).unwrap(), snapshot(&remote).unwrap());
        assert!(!remote.join("target").exists());
        assert!(!remote.join(".ssh").exists());
        for name in SECRET_NAMES {
            assert_eq!(
                std::fs::read_to_string(remote.join(name)).unwrap(),
                "remote secret"
            );
        }
        std::fs::remove_file(local.join("source-0")).unwrap();
        rsync(&format!("{}/", local.display()), &remote_spec, Some(&shell))
            .await
            .unwrap();
        assert!(!remote.join("source-0").exists());
        std::fs::remove_file(remote.join("source-1")).unwrap();
        let def: TargetDef = serde_json::from_value(json!({
            "name": "fixture", "kind": "ssh", "address": format!("{user}@127.0.0.1"), "port": port
        }))
        .unwrap();
        let plan = ssh_plan(
            "ssh",
            &def,
            Some(&key),
            &fixture.path().join("known_hosts"),
            &RemoteCmd {
                cwd: Some(remote.display().to_string()),
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "mkdir -p target && cc hello.c -o target/hello && ./target/hello > result"
                        .into(),
                ],
                ..RemoteCmd::default()
            },
        )
        .unwrap();
        let mut command = tokio::process::Command::new(plan.program);
        command.args(plan.args);
        let built = pump_process(command, None, Some(Duration::from_secs(30)), None)
            .collect(4096)
            .await;
        assert!(
            built.end().success(),
            "fixture build failed: {:?}",
            built.end()
        );
        let sync = Sync {
            root: local.clone(),
            remote: remote_spec,
            shell,
            before: snapshot(&local).unwrap(),
            _key: None,
            _lock: workspace_lock(&local).try_lock_owned().unwrap(),
        };
        sync.pull().await.unwrap();
        let stage = tempfile::tempdir().unwrap();
        let stats = sync.download(stage.path()).await.unwrap();
        assert!(
            stats.contains("Number of regular files transferred: 0"),
            "{stats}"
        );
        for name in SECRET_NAMES {
            assert_eq!(
                std::fs::read_to_string(local.join(name)).unwrap(),
                "local secret"
            );
        }
        assert!(!local.join("source-1").exists());
        assert_eq!(
            std::fs::read_to_string(local.join("result")).unwrap(),
            "build output"
        );
        assert!(local.join("target/cache").exists());
        let sync = Sync {
            before: snapshot(&local).unwrap(),
            ..sync
        };
        std::fs::write(local.join("harness-edit"), "keep me").unwrap();
        std::fs::write(remote.join("result"), "new output").unwrap();
        let error = sync.pull().await.unwrap_err();
        assert!(
            error
                .message
                .contains("sync_conflict: local edits in harness-edit")
        );
        assert_eq!(
            std::fs::read_to_string(local.join("result")).unwrap(),
            "build output"
        );
        assert_eq!(
            std::fs::read_to_string(remote.join("result")).unwrap(),
            "new output"
        );
        server.kill().await.unwrap();
    }
}
