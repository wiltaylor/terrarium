//! API coverage using a private sshd with a chroot filesystem. The remote
//! workspace has the same absolute path without sharing the daemon's files.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn copy_binary(jail: &Path, binary: &str) {
    let copy = |source: &Path| {
        let dest = jail.join(source.strip_prefix("/").unwrap());
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(source, dest).unwrap();
    };
    copy(Path::new(binary));
    let libs = std::process::Command::new("ldd")
        .arg(binary)
        .output()
        .unwrap();
    assert!(libs.status.success());
    for word in String::from_utf8(libs.stdout).unwrap().split_whitespace() {
        if word.starts_with('/') {
            copy(Path::new(word));
        }
    }
}

fn copy_python(jail: &Path) {
    copy_binary(jail, "/usr/bin/python3");
    // -S and the path helper use frozen os/stat/sys. Startup still needs the
    // interpreter's encodings package. Copy only that package into the jail.
    let encodings = std::process::Command::new("/usr/bin/python3")
        .args(["-S", "-c", "import encodings; print(encodings.__path__[0])"])
        .output()
        .unwrap();
    assert!(encodings.status.success());
    let path = PathBuf::from(String::from_utf8(encodings.stdout).unwrap().trim());
    let dest = jail.join(path.strip_prefix("/").unwrap());
    std::fs::create_dir_all(&dest).unwrap();
    // Python's bootstrap uses these landmarks to resolve its stdlib prefix.
    let library = path.parent().unwrap();
    std::fs::copy(library.join("os.py"), dest.parent().unwrap().join("os.py")).unwrap();
    std::fs::create_dir(dest.parent().unwrap().join("lib-dynload")).unwrap();
    for entry in std::fs::read_dir(path).unwrap().flatten() {
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "py")
        {
            std::fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
        }
    }
}

pub(crate) struct Ssh {
    _dir: tempfile::TempDir,
    pub(crate) jail: PathBuf,
    pub(crate) key: PathBuf,
    pub(crate) port: u16,
    pub(crate) server: tokio::process::Child,
}

impl Ssh {
    pub(crate) async fn start(sshd: std::ffi::OsString) -> Self {
        // sshd rejects a writable ancestor such as /tmp for ChrootDirectory.
        let dir = tempfile::tempdir_in("/run").unwrap();
        let jail = dir.path().join("jail");
        std::fs::create_dir(&jail).unwrap();
        std::fs::set_permissions(&jail, std::fs::Permissions::from_mode(0o755)).unwrap();
        for binary in [
            "/bin/sh",
            "/bin/bash",
            "/usr/bin/rsync",
            "/bin/cat",
            "/bin/mkdir",
            "/bin/mv",
            "/bin/rm",
            "/bin/ln",
            "/usr/bin/wc",
            "/bin/chmod",
            "/usr/bin/dirname",
            "/bin/sleep",
            "/usr/bin/base64",
            "/usr/bin/tr",
            "/usr/bin/setsid",
            "/usr/bin/flock",
            "/usr/bin/nohup",
            "/usr/bin/head",
            "/usr/bin/tail",
            "/usr/bin/mkfifo",
            "/usr/bin/rmdir",
            "/usr/bin/timeout",
            "/usr/bin/date",
        ] {
            copy_binary(&jail, binary);
        }
        copy_python(&jail);
        let probe = std::process::Command::new("chroot")
            .arg(&jail)
            .args([
                "/usr/bin/python3",
                "-S",
                "-c",
                "import os, stat, sys; assert hasattr(os, 'O_NOFOLLOW')",
            ])
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "fixture Python bootstrap: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        std::fs::rename(jail.join("usr/bin/rsync"), jail.join("usr/bin/rsync-real")).unwrap();
        std::fs::write(
            jail.join("usr/bin/rsync"),
            "#!/bin/sh\nexec /usr/bin/rsync-real --debug=DELTASUM3 --msgs2stderr \"$@\" 2>/tmp/rsync-debug\n",
        )
        .unwrap();
        std::fs::set_permissions(
            jail.join("usr/bin/rsync"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        for name in ["tmp", "root", "dev", "etc"] {
            std::fs::create_dir(jail.join(name)).unwrap();
        }
        std::fs::write(jail.join("dev/null"), "").unwrap();
        let key = dir.path().join("key");
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
        let config = dir.path().join("config");
        std::fs::write(&config, format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin yes\nChrootDirectory {}\nPidFile {}/pid\n", key.display(), key.display(), jail.display(), dir.path().display())).unwrap();
        let mut server = tokio::process::Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(config)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            assert!(server.try_wait().unwrap().is_none(), "private sshd exited");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            _dir: dir,
            jail,
            key,
            port,
            server,
        }
    }
}

async fn done(td: &TestDaemon, sid: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut since = 0;
        loop {
            let r = td
                .get(&format!(
                    "/api/targets/synced/sessions/{sid}/output?since={since}&wait=1"
                ))
                .await;
            let body = r.ok();
            since = body["next"].as_u64().unwrap();
            if body["done"] == true {
                return body["end"].clone();
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn ssh_api_exec_run_copy_and_conflict() {
    // Explicit opt-in: the fixture needs root for chroot, openssh-server,
    // openssh-client, rsync, and the standard dynamically linked Linux tools.
    let Some(sshd) = std::env::var_os("TERRARIUM_TEST_SSHD") else {
        return;
    };
    // Keep the production security floor while allowing older test hosts to skip.
    let version_output = tokio::process::Command::new("rsync")
        .arg("--version")
        .output()
        .await;
    let supported = version_output.is_ok_and(|output| {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let version = stdout
            .strip_prefix("rsync  version ")
            .and_then(|s| s.split_whitespace().next())
            .map(|s| s.split('.').map(str::parse::<u32>).collect::<Vec<_>>());
        output.status.success()
            && matches!(version.as_deref(), Some([Ok(major), Ok(minor), Ok(patch)]) if (*major, *minor, *patch) >= (3, 4, 0))
    });
    if !supported {
        eprintln!("SKIP: ssh_api_exec_run_copy_and_conflict requires rsync >= 3.4.0 on PATH");
        return;
    }
    let mut ssh = Ssh::start(sshd).await;
    let probe = tokio::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-i",
        ])
        .arg(&ssh.key)
        .arg("-p")
        .arg(ssh.port.to_string())
        .args(["root@127.0.0.1", "mkdir -p /tmp/probe"])
        .output()
        .await
        .unwrap();
    assert!(
        probe.status.success(),
        "chroot sshd fixture: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.post(
        "/api/secrets",
        json!({"name":"sync-key", "kind":"ssh_key", "fields":{
            "private_key":std::fs::read_to_string(&ssh.key).unwrap()
        }}),
    )
    .await
    .ok();
    td.post("/api/targets", json!({"name":"synced", "kind":"ssh", "address":"root@127.0.0.1", "port":ssh.port, "credential":"sync-key", "sync_workspace":true})).await.ok();
    let agent = td
        .agent("p", "worker", json!({"policy":{"targets":["synced"]}}))
        .await;
    let agent = agent["id"].as_str().unwrap();
    let root = td.state.with_file_name("work").join("p/worker");
    std::fs::create_dir_all(&root).unwrap();
    let remote = ssh.jail.join(root.strip_prefix("/").unwrap());
    std::fs::write(root.join("input"), "pushed").unwrap();
    std::fs::write(root.join("id_parser.rs"), "source").unwrap();
    for name in [
        "id_rsa",
        "id_ed25519.pub",
        "id_ecdsa",
        "id_dsa.pub",
        "id_ed25519_sk",
        "id_ed25519_sk.pub",
        "id_ecdsa_sk",
        "id_ecdsa_sk.pub",
    ] {
        std::fs::write(root.join(name), "excluded").unwrap();
    }
    std::fs::write(root.join(".env"), "never ship").unwrap();
    let started = td
        .post(
            "/api/targets/synced/exec",
            json!({"agent":agent, "cwd":root, "cmd":["sh","-c","cat input > output; sleep 1"]}),
        )
        .await;
    let sid = started.ok()["session_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !remote.join("output").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Plant hostile remote output while the command is still running.
    std::fs::write(remote.join("privileged"), "not privileged locally").unwrap();
    std::fs::set_permissions(
        remote.join("privileged"),
        std::fs::Permissions::from_mode(0o6755),
    )
    .unwrap();
    std::fs::create_dir(remote.join("privileged-dir")).unwrap();
    std::fs::set_permissions(
        remote.join("privileged-dir"),
        std::fs::Permissions::from_mode(0o2775),
    )
    .unwrap();
    for args in [
        vec!["mkfifo", "pipe"],
        vec!["mknod", "device", "c", "1", "3"],
    ] {
        assert!(
            std::process::Command::new(args[0])
                .args(&args[1..])
                .current_dir(&remote)
                .status()
                .unwrap()
                .success()
        );
    }
    assert!(!root.join("output").exists());
    assert_eq!(
        td.get(&format!("/api/targets/synced/sessions/{sid}"))
            .await
            .ok()["done"],
        false
    );
    assert_eq!(done(&td, sid).await["code"], 0);
    assert_eq!(
        std::fs::read_to_string(root.join("output")).unwrap(),
        "pushed"
    );
    assert!(!remote.join(".env").exists());
    assert_eq!(
        std::fs::read_to_string(remote.join("id_parser.rs")).unwrap(),
        "source"
    );
    for name in [
        "id_rsa",
        "id_ed25519.pub",
        "id_ecdsa",
        "id_dsa.pub",
        "id_ed25519_sk",
        "id_ed25519_sk.pub",
        "id_ecdsa_sk",
        "id_ecdsa_sk.pub",
    ] {
        assert!(!remote.join(name).exists());
    }
    for name in ["privileged", "privileged-dir"] {
        assert_eq!(
            std::fs::metadata(root.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o6000,
            0
        );
    }
    assert!(!root.join("pipe").exists());
    assert!(!root.join("device").exists());

    // A first pull plants a directory symlink. The next pull replaces it
    // with a directory containing an attacker guess at an outside file.
    let outside = td.dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(
        outside.join("secret"),
        format!("{}private", "Q".repeat(65536)),
    )
    .unwrap();
    std::fs::write(root.join("guess"), format!("{}guess", "Q".repeat(65536))).unwrap();
    let started = td
        .post(
            "/api/targets/synced/exec",
            json!({"agent":agent,"cwd":root,"cmd":["ln","-s",outside,"escape"]}),
        )
        .await;
    assert_eq!(
        done(&td, started.ok()["session_id"].as_str().unwrap()).await["code"],
        0
    );
    assert!(root.join("escape").is_symlink());
    let started = td.post("/api/targets/synced/exec", json!({"agent":agent,"cwd":root,"cmd":["sh","-c","rm escape; mkdir escape; cat guess > escape/secret"]})).await;
    assert_eq!(
        done(&td, started.ok()["session_id"].as_str().unwrap()).await["code"],
        0
    );
    let debug = std::fs::read_to_string(ssh.jail.join("tmp/rsync-debug")).unwrap();
    let counts: Vec<_> = debug
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("count="))
        .collect();
    assert!(
        !counts.is_empty(),
        "missing sender checksum diagnostics: {debug}"
    );
    assert!(
        counts.iter().all(|count| *count == "0"),
        "outside-file block checksums leaked: {debug}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("secret")).unwrap(),
        format!("{}private", "Q".repeat(65536))
    );

    // Negative control: modern rsync rejects symlinked basis paths before
    // sending checksums. Model the exposed file as a regular basis file so
    // this control reaches the checksum assertion even on patched rsync.
    let unsafe_basis = tempfile::tempdir().unwrap();
    std::fs::create_dir(unsafe_basis.path().join("escape")).unwrap();
    std::fs::copy(
        outside.join("secret"),
        unsafe_basis.path().join("escape/secret"),
    )
    .unwrap();
    let stage = tempfile::tempdir().unwrap();
    let shell = format!(
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i {} -p {}",
        ssh.key.display(),
        ssh.port
    );
    let control = tokio::process::Command::new("rsync")
        .args(["-a", "--checksum", "--no-whole-file", "--copy-dest"])
        .arg(unsafe_basis.path().join("escape"))
        .args(["-e", &shell])
        .arg(format!("root@127.0.0.1:{}/escape/", root.display()))
        .arg(format!("{}/escape/", stage.path().display()))
        .output()
        .await
        .unwrap();
    assert!(
        control.status.success(),
        "negative control: {}",
        String::from_utf8_lossy(&control.stderr)
    );
    let debug = std::fs::read_to_string(ssh.jail.join("tmp/rsync-debug")).unwrap();
    assert!(
        debug
            .split_whitespace()
            .filter_map(|word| word.strip_prefix("count="))
            .any(|count| count != "0"),
        "negative control did not expose outside-file checksums: {debug}"
    );

    let run = td
        .post_as(
            agent,
            &format!("/api/agents/{agent}/run"),
            json!({"target":"synced", "cwd":root, "cmd":["sh","-c","cat output > run-output"]}),
        )
        .await;
    let sid = run.ok()["session_id"].as_str().unwrap();
    assert_eq!(done(&td, sid).await["code"], 0);
    assert_eq!(
        std::fs::read_to_string(root.join("run-output")).unwrap(),
        "pushed"
    );

    let cp = td
        .post(
            "/api/targets/synced/cp",
            json!({"agent":agent,"direction":"push","path":root.join("copied")}),
        )
        .await;
    let sid = cp.ok()["session_id"].as_str().unwrap();
    let bytes = b"copied data";
    td.post(&format!("/api/targets/synced/sessions/{sid}/input"), json!({"data":base64::engine::general_purpose::STANDARD.encode(bytes),"finish":{"bytes":bytes.len(),"sha256":terrarium_host::protocol::hex(&Sha256::digest(bytes))}})).await.ok();
    let end = done(&td, sid).await;
    assert_eq!(end["type"], "cp", "{end}");
    assert_eq!(std::fs::read(root.join("copied")).unwrap(), bytes);

    let cp = td
        .post(
            "/api/targets/synced/cp",
            json!({"agent":agent,"direction":"pull","path":root.join("copied")}),
        )
        .await;
    let sid = cp.ok()["session_id"].as_str().unwrap();
    assert_eq!(done(&td, sid).await["type"], "cp");

    let started = td
        .post(
            "/api/targets/synced/exec",
            json!({"agent":agent,"cwd":root,"cmd":["sh","-c","echo remote > conflict; sleep 1"]}),
        )
        .await;
    let sid = started.ok()["session_id"].as_str().unwrap();
    std::fs::write(root.join("conflict"), "harness").unwrap();
    let end = done(&td, sid).await;
    assert_eq!(end["code"], "sync_conflict");
    assert!(end["message"].as_str().unwrap().contains("conflict"));
    assert_eq!(
        std::fs::read_to_string(root.join("conflict")).unwrap(),
        "harness"
    );
    assert_eq!(
        std::fs::read_to_string(remote.join("conflict")).unwrap(),
        "remote\n"
    );
    ssh.server.kill().await.unwrap();
}
