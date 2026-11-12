//! Runtime identity acquired from Docker and rechecked around proc FD pinning.
//! Numeric PIDs are never accepted from the wire. Namespace ownership is tied
//! to the same full container ID, start timestamp and process start ticks.
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use terrarium_core::live_workspace::{MountEvidence, NamespaceIdentity, Reply, RuntimeIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub identity: RuntimeIdentity,
    pub pid: u32,
    pub started_at: String,
    pub running: bool,
}

/// An inspector queries the local Docker daemon using the full immutable ID.
/// Its values are not client-supplied registration evidence. Configuration
/// admission belongs to B3 in addition to these identity checks.
pub trait Inspector {
    fn inspect(&mut self, container_id: &str) -> Result<Snapshot, Reply>;
}

pub struct PinnedRuntime {
    pub namespace: File,
    pub root: File,
    proc: File,
    snapshot: Snapshot,
    start_ticks: u64,
}

fn read_at(parent: &File, name: &str, limit: u64) -> Result<Vec<u8>, Reply> {
    let file = File::from(
        openat(
            parent,
            name,
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| Reply::Quarantined)?,
    );
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Reply::Quarantined)?;
    if bytes.len() as u64 > limit {
        return Err(Reply::Quarantined);
    }
    Ok(bytes)
}

fn ticks(proc: &File) -> Result<u64, Reply> {
    let bytes = read_at(proc, "stat", 4096)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Reply::Quarantined)?;
    let (_, tail) = text.rsplit_once(')').ok_or(Reply::Quarantined)?;
    // tail begins at field 3 (state). starttime is field 22.
    tail.split_whitespace()
        .nth(19)
        .ok_or(Reply::Quarantined)?
        .parse()
        .map_err(|_| Reply::Quarantined)
}

fn identity(fd: &File) -> Result<NamespaceIdentity, Reply> {
    let m = fd.metadata().map_err(|_| Reply::Quarantined)?;
    Ok(NamespaceIdentity {
        device: m.dev(),
        inode: m.ino(),
    })
}

impl PinnedRuntime {
    pub fn identity(&self) -> &RuntimeIdentity {
        &self.snapshot.identity
    }
    pub fn acquire(
        inspector: &mut impl Inspector,
        expected: &RuntimeIdentity,
    ) -> Result<Self, Reply> {
        if expected.container_id.len() != 64
            || !expected.container_id.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Reply::Unauthorized);
        }
        let first = inspector.inspect(&expected.container_id)?;
        if first.identity != *expected
            || !first.running
            || first.pid == 0
            || first.started_at.is_empty()
            || first.started_at.len() > 128
        {
            return Err(Reply::Quarantined);
        }
        let proc_root = File::open("/proc").map_err(|_| Reply::Failed)?;
        if nix::sys::statfs::fstatfs(&proc_root)
            .map_err(|_| Reply::Failed)?
            .filesystem_type()
            != nix::sys::statfs::PROC_SUPER_MAGIC
        {
            return Err(Reply::Quarantined);
        }
        let proc = File::from(
            openat(
                &proc_root,
                first.pid.to_string().as_str(),
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|_| Reply::Quarantined)?,
        );
        let start_ticks = ticks(&proc)?;
        // These two magic links are kernel-provided inside the pinned procfs
        // directory, never workspace paths. No user-selected namespace path.
        let namespace = File::from(
            openat(
                &proc,
                "ns/mnt",
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| Reply::Quarantined)?,
        );
        let root = File::from(
            openat(
                &proc,
                "root",
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| Reply::Quarantined)?,
        );
        let pin = Self {
            namespace,
            root,
            proc,
            snapshot: first,
            start_ticks,
        };
        pin.recheck(inspector, expected)?;
        Ok(pin)
    }

    pub fn recheck(
        &self,
        inspector: &mut impl Inspector,
        expected: &RuntimeIdentity,
    ) -> Result<(), Reply> {
        let current = inspector.inspect(&expected.container_id)?;
        let live_ns = File::from(
            openat(
                &self.proc,
                "ns/mnt",
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| Reply::Quarantined)?,
        );
        if current != self.snapshot
            || current.identity != *expected
            || !current.running
            || ticks(&self.proc)? != self.start_ticks
            || identity(&self.namespace)? != expected.namespace
            || identity(&live_ns)? != expected.namespace
        {
            return Err(Reply::Quarantined);
        }
        Ok(())
    }

    /// Read only from the pinned process directory and recheck runtime on both
    /// sides. Mount evidence is local to this pinned namespace.
    pub fn mountinfo(&self, inspector: &mut impl Inspector) -> Result<Vec<MountEvidence>, Reply> {
        self.recheck(inspector, &self.snapshot.identity)?;
        let bytes = read_at(&self.proc, "mountinfo", 4 * 1024 * 1024)?;
        let records = super::mountinfo::parse(&bytes, self.snapshot.identity.namespace.clone())?;
        self.recheck(inspector, &self.snapshot.identity)?;
        Ok(records)
    }
}

/// Fixed local Docker inspect command. No PATH, inherited environment, shell,
/// user-provided Docker flags or namespace PID. Both executable and output are
/// pinned/bounded, and timeout cleanup affects only the child started here.
pub struct DockerInspector;

impl DockerInspector {
    fn output(container_id: &str) -> Result<Vec<u8>, Reply> {
        if container_id.len() != 64 || !container_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Reply::Unauthorized);
        }
        Self::output_args(&["inspect", "--type", "container", container_id])
    }

    /// Inspect only the helper-selected digest, then the already bound full
    /// container ID. This checks configuration, not B3 mount evidence. Neither
    /// image tags nor arbitrary CLI arguments are accepted from the request.
    pub fn verify_receiver(
        &mut self,
        specification: &super::receiver::Specification,
        request: &terrarium_core::live_workspace::bootstrap::Request,
        measured: &crate::docker::live_workspace::Profile,
        expected: &RuntimeIdentity,
    ) -> Result<Snapshot, Reply> {
        specification.verify(request, measured)?;
        if expected.registration != request.registration || expected.incarnation != request.incarnation {
            return Err(Reply::Quarantined);
        }
        let image = Self::output_args(&["image", "inspect", specification.image_reference()])?;
        let image_id = Self::resolved_image(&image, specification.image_reference())?;
        let bytes = Self::output(&expected.container_id)?;
        specification.check_inspect_configuration(request, measured, &expected.container_id, &image_id, &bytes)?;
        let snapshot = Self::parse(&bytes, &expected.container_id)?;
        if snapshot.identity != *expected {
            return Err(Reply::Quarantined);
        }
        Ok(snapshot)
    }

    fn resolved_image(bytes: &[u8], reference: &str) -> Result<String, Reply> {
        let rows: Vec<serde_json::Value> = serde_json::from_slice(bytes).map_err(|_| Reply::Failed)?;
        let [row] = rows.as_slice() else { return Err(Reply::Quarantined) };
        let id = row["Id"].as_str().ok_or(Reply::Quarantined)?;
        if !id.strip_prefix("sha256:").is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
            || !row["RepoDigests"].as_array().is_some_and(|digests| digests.iter().any(|v| v.as_str() == Some(reference)))
        {
            return Err(Reply::Quarantined);
        }
        Ok(id.into())
    }

    // Private closed call sites above supply the verb and arguments. Keep this
    // runner unavailable to the wire protocol and the unprivileged forwarder.
    fn output_args(args: &[&str]) -> Result<Vec<u8>, Reply> {
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let bin = super::ProtectedRoot::open(std::path::Path::new("/usr/bin"))?;
        let executable = File::from(
            openat(
                &bin.fd,
                "docker",
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
                Mode::empty(),
            )
            .map_err(|_| Reply::Unsupported)?,
        );
        let m = executable.metadata().map_err(|_| Reply::Failed)?;
        if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 || m.mode() & 0o111 == 0 {
            return Err(Reply::Unauthorized);
        }
        let mut child = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()))
            .args([
                "--host",
                "unix:///var/run/docker.sock",
            ])
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Reply::Failed)?;
        let result = (|| {
            let mut stdout = child.stdout.take().ok_or(Reply::Failed)?;
            let flags = OFlag::from_bits_truncate(
                nix::fcntl::fcntl(&stdout, nix::fcntl::FcntlArg::F_GETFL)
                    .map_err(|_| Reply::Failed)?,
            );
            nix::fcntl::fcntl(
                &stdout,
                nix::fcntl::FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK),
            )
            .map_err(|_| Reply::Failed)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut out = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                if Instant::now() >= deadline {
                    return Err(Reply::Failed);
                }
                match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        out.extend_from_slice(&buffer[..n]);
                        if out.len() > 256 * 1024 {
                            return Err(Reply::Failed);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => return Err(Reply::Failed),
                }
            }
            loop {
                if let Some(status) = child.try_wait().map_err(|_| Reply::Failed)? {
                    return if status.success() {
                        Ok(out)
                    } else {
                        Err(Reply::Failed)
                    };
                }
                if Instant::now() >= deadline {
                    return Err(Reply::Failed);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }

    fn parse(bytes: &[u8], container_id: &str) -> Result<Snapshot, Reply> {
        let values: Vec<serde_json::Value> =
            serde_json::from_slice(bytes).map_err(|_| Reply::Failed)?;
        if values.len() != 1 {
            return Err(Reply::Quarantined);
        }
        let v = &values[0];
        let id = v["Id"].as_str().ok_or(Reply::Failed)?;
        if id != container_id || id.len() != 64 {
            return Err(Reply::Quarantined);
        }
        let labels = &v["Config"]["Labels"];
        let registration =
            serde_json::from_value(labels["terrarium.workspace.registration"].clone())
                .map_err(|_| Reply::Quarantined)?;
        let incarnation = serde_json::from_value(labels["terrarium.workspace.runtime"].clone())
            .map_err(|_| Reply::Quarantined)?;
        let pid = u32::try_from(v["State"]["Pid"].as_u64().ok_or(Reply::Quarantined)?)
            .map_err(|_| Reply::Quarantined)?;
        if pid == 0
            || v["State"]["Running"] != true
            || v["State"]["Paused"] != false
            || v["State"]["Restarting"] != false
        {
            return Err(Reply::Quarantined);
        }
        let ns =
            std::fs::metadata(format!("/proc/{pid}/ns/mnt")).map_err(|_| Reply::Quarantined)?;
        let started_at = v["State"]["StartedAt"]
            .as_str()
            .ok_or(Reply::Quarantined)?
            .to_owned();
        if started_at.is_empty() || started_at.len() > 128 {
            return Err(Reply::Quarantined);
        }
        Ok(Snapshot {
            identity: RuntimeIdentity {
                registration,
                incarnation,
                container_id: id.into(),
                namespace: NamespaceIdentity {
                    device: ns.dev(),
                    inode: ns.ino(),
                },
            },
            pid,
            started_at,
            running: true,
        })
    }
}

impl Inspector for DockerInspector {
    fn inspect(&mut self, container_id: &str) -> Result<Snapshot, Reply> {
        Self::parse(&Self::output(container_id)?, container_id)
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;

    #[test]
    fn image_resolution_refuses_tags_missing_digest_and_ambiguous_rows() {
        let reference = format!("synthetic@sha256:{}", "a".repeat(64));
        let id = format!("sha256:{}", "b".repeat(64));
        let row = serde_json::json!({"Id": id, "RepoDigests": [reference]});
        let check = |value: serde_json::Value| {
            DockerInspector::resolved_image(value.to_string().as_bytes(), &reference)
        };
        assert_eq!(check(serde_json::json!([row.clone()])), Ok(id));
        for value in [
            serde_json::json!([]),
            serde_json::json!([row.clone(), row.clone()]),
            serde_json::json!([{"Id": "sha256:short", "RepoDigests": [reference]}]),
            serde_json::json!([{"Id": row["Id"], "RepoTags": [reference]}]),
            serde_json::json!([{"Id": row["Id"], "RepoDigests": ["synthetic:latest"]}]),
            serde_json::json!([{"Id": row["Id"], "RepoDigests": null}]),
        ] {
            assert_eq!(check(value), Err(Reply::Quarantined));
        }
    }
}
