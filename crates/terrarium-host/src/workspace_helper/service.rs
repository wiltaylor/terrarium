//! Separate root process, never launched by the self-updating host agent.
//! It only opens administrator-provisioned journals. There is no registration,
//! target creation, opt-in, arbitrary host verb, or enabled daemon gate here.
use super::*;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use terrarium_core::live_workspace::{Capability, PROTOCOL_VERSION, STATE_VERSION};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    host_agent_uid: u32,
    host_agent_gid: u32,
    work_root: PathBuf,
    registrations: Vec<Registration>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    publisher_root: PathBuf,
    staging_root: PathBuf,
    state_root: PathBuf,
}

fn failed() -> std::io::Error {
    std::io::Error::other("workspace helper refused protected configuration, state or runtime")
}

fn config() -> Result<Config, Reply> {
    use nix::fcntl::{OFlag, openat};
    let root = ProtectedRoot::open(std::path::Path::new("/etc/terrarium-workspace-helper"))?;
    let file = File::from(
        openat(
            &root.fd,
            "config.json",
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|_| Reply::Failed)?,
    );
    let m = file.metadata().map_err(|_| Reply::Failed)?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 || m.nlink() != 1 {
        return Err(Reply::Unauthorized);
    }
    let mut bytes = Vec::new();
    file.take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Reply::Failed)?;
    parse_config(&bytes)
}

fn parse_config(bytes: &[u8]) -> Result<Config, Reply> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Reply::Failed);
    }
    let config: Config = serde_json::from_slice(bytes).map_err(|_| Reply::Failed)?;
    if config.host_agent_uid == 0
        || config.registrations.len() > 128
        || !config.work_root.is_absolute()
        || config.registrations.iter().any(|r| {
            !r.publisher_root.is_absolute()
                || !r.staging_root.is_absolute()
                || !r.state_root.is_absolute()
        })
    {
        return Err(Reply::Unauthorized);
    }
    Ok(config)
}

/// No compatibility or hello response activates admission. B4 must explicitly
/// replace the disabled daemon gate after its authority/security review.
pub fn capability() -> Capability {
    Capability {
        version: PROTOCOL_VERSION,
        state_version: STATE_VERSION,
        workspace_mount: true,
        ready: false,
        reason: "daemon_activation_disabled".into(),
    }
}

pub async fn run() -> std::io::Result<()> {
    use nix::fcntl::AtFlags;
    use nix::sys::stat::{SFlag, fstatat};
    use nix::unistd::{Gid, Uid, UnlinkatFlags, fchownat, unlinkat};
    if !nix::unistd::geteuid().is_root() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "workspace helper requires root",
        ));
    }
    let c = config().map_err(|_| failed())?;
    let mut helpers = BTreeMap::new();
    let mut administrative_roots = std::collections::BTreeSet::new();
    // Take every journal lock before touching the socket. A second process can
    // neither replace a live listener nor mutate this registration stream.
    for r in c.registrations {
        let store = Store::open(ProtectedRoot::open(&r.state_root).map_err(|_| failed())?)
            .map_err(|_| failed())?;
        let journal = store.load().map_err(|_| failed())?;
        let registration = journal.runtime.registration;
        let mut docker = runtime::DockerInspector;
        let pin =
            runtime::PinnedRuntime::acquire(&mut docker, &journal.runtime).map_err(|_| failed())?;
        let source = ProtectedRoot::open(&c.work_root).map_err(|_| failed())?;
        let publisher = ProtectedRoot::open(&r.publisher_root).map_err(|_| failed())?;
        let staging = ProtectedRoot::open(&r.staging_root).map_err(|_| failed())?;
        for root in [&publisher, &staging] {
            let metadata = root.fd.metadata()?;
            if !administrative_roots.insert((metadata.dev(), metadata.ino())) {
                return Err(failed());
            }
        }
        let kernel = kernel::Kernel::new(
            source,
            publisher,
            staging,
            pin,
            docker,
            mounts::LinuxAttributes,
        )
        .map_err(|_| failed())?;
        let helper = Helper::open(store, kernel).map_err(|_| failed())?;
        if helpers.insert(registration, helper).is_some() {
            return Err(failed());
        }
    }
    let socket_root = ProtectedRoot::open(std::path::Path::new("/run/terrarium-workspace-helper"))
        .map_err(|_| failed())?;
    let metadata = socket_root.fd.metadata()?;
    if metadata.gid() != c.host_agent_gid || metadata.mode() & 0o777 != 0o750 {
        return Err(failed());
    }
    let socket = format!("/proc/self/fd/{}/socket", socket_root.fd.as_raw_fd());
    match fstatat(&socket_root.fd, "socket", AtFlags::AT_SYMLINK_NOFOLLOW) {
        Err(nix::errno::Errno::ENOENT) => {}
        Ok(m)
            if SFlag::from_bits_truncate(m.st_mode) & SFlag::S_IFMT == SFlag::S_IFSOCK
                && m.st_uid == 0
                && m.st_gid == c.host_agent_gid
                && m.st_mode & 0o777 == 0o660
                && m.st_nlink == 1 =>
        {
            match tokio::time::timeout(
                std::time::Duration::from_millis(250),
                UnixStream::connect(&socket),
            )
            .await
            {
                Ok(Err(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    unlinkat(&socket_root.fd, "socket", UnlinkatFlags::NoRemoveDir)
                        .map_err(|_| failed())?
                }
                _ => return Err(failed()),
            }
        }
        _ => return Err(failed()),
    }
    let listener = tokio::net::UnixListener::bind(&socket)?;
    fchownat(
        &socket_root.fd,
        "socket",
        Some(Uid::from_raw(0)),
        Some(Gid::from_raw(c.host_agent_gid)),
        AtFlags::AT_SYMLINK_NOFOLLOW,
    )
    .map_err(|_| failed())?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))?;
    socket_root.fd.sync_all()?;
    loop {
        let (stream, _) = listener.accept().await?;
        // Admission remains serialized per registration. No task can outlive
        // the held journal/namespace FDs or race another operation on a target.
        let result = serve_frame(stream, c.host_agent_uid, |bytes| {
            let e = decode_frame(bytes)?;
            helpers
                .get_mut(&e.original.runtime.registration)
                .ok_or(Reply::Unsupported)?
                .apply(bytes)
        })
        .await;
        if result.is_err() {
            tracing::warn!("workspace helper connection failed or timed out");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_is_bounded_strict_and_does_not_activate() {
        assert!(!capability().ready);
        assert!(!capability().compatible());
        assert!(parse_config(br#"{"host_agent_uid":1000,"host_agent_gid":1000,"work_root":"/work","registrations":[]}"#).is_ok());
        assert!(parse_config(br#"{"host_agent_uid":0,"host_agent_gid":1000,"work_root":"/work","registrations":[]}"#).is_err());
        assert!(parse_config(br#"{"host_agent_uid":1000,"host_agent_gid":1000,"work_root":"relative","registrations":[]}"#).is_err());
        assert!(parse_config(br#"{"host_agent_uid":1000,"host_agent_gid":1000,"work_root":"/work","registrations":[],"flags":1}"#).is_err());
        assert!(parse_config(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
    }
}
