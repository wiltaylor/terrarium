//! Separate root process, never launched by the self-updating host agent.
//! It only opens bootstrap-owned journals. There is no registration,
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

#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Config {
    host_agent_uid: u32,
    host_agent_gid: u32,
    work_root: PathBuf,
    registrations: Vec<Registration>,
    #[serde(default)]
    receiver: Option<receiver::Specification>,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Registration {
    publisher_root: PathBuf,
    staging_root: PathBuf,
    state_root: PathBuf,
}

impl Registration {
    fn id(&self) -> Result<terrarium_core::live_workspace::RegistrationId, Reply> {
        let component = self
            .state_root
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or(Reply::Unauthorized)?;
        let id: terrarium_core::live_workspace::RegistrationId =
            serde_json::from_value(serde_json::Value::String(component.into()))
                .map_err(|_| Reply::Unauthorized)?;
        for (path, parent) in [
            (&self.publisher_root, "publishers"),
            (&self.staging_root, "staging"),
            (&self.state_root, "registrations"),
        ] {
            if path.to_str()
                != Some(format!("/var/lib/terrarium-workspace-helper/{parent}/{}", id.0).as_str())
            {
                return Err(Reply::Unauthorized);
            }
        }
        Ok(id)
    }
}

fn failed() -> std::io::Error {
    std::io::Error::other("workspace helper refused protected configuration, state or runtime")
}

fn config() -> Result<(Config, ProtectedRoot, File), Reply> {
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
    (&file)
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Reply::Failed)?;
    Ok((parse_config(&bytes)?, root, file))
}

fn parse_config(bytes: &[u8]) -> Result<Config, Reply> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Reply::Failed);
    }
    let config: Config = serde_json::from_slice(bytes).map_err(|_| Reply::Failed)?;
    if config.host_agent_uid == 0
        || (!config.registrations.is_empty() && config.receiver.is_none())
        || config.receiver.as_ref().is_some_and(|s| s.digest().is_err())
        || config.registrations.len() > 128
        || !config.work_root.is_absolute()
        || [
            "/etc/terrarium-workspace-helper",
            "/run/terrarium-workspace-helper",
        ]
        .iter()
        .any(|p| {
            std::path::Path::new(p).starts_with(&config.work_root)
                || config.work_root.starts_with(p)
        })
        || config.registrations.iter().any(|r| {
            r.id().is_err()
                || !r.publisher_root.is_absolute()
                || !r.staging_root.is_absolute()
                || !r.state_root.is_absolute()
                || [&r.publisher_root, &r.staging_root, &r.state_root]
                    .iter()
                    .any(|p| p.starts_with(&config.work_root) || config.work_root.starts_with(p))
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
    let (c, config_root, _config_file) = config().map_err(|_| failed())?;
    let source = ProtectedRoot::open(&c.work_root).map_err(|_| failed())?;
    let socket_root = ProtectedRoot::open(std::path::Path::new("/run/terrarium-workspace-helper"))
        .map_err(|_| failed())?;
    let metadata = socket_root.fd.metadata()?;
    if metadata.gid() != c.host_agent_gid || metadata.mode() & 0o777 != 0o750 {
        return Err(failed());
    }
    struct Opened {
        publisher: ProtectedRoot,
        staging: ProtectedRoot,
        state: ProtectedRoot,
        bootstrap: ProtectedRoot,
    }
    let mut opened = Vec::new();
    for r in &c.registrations {
        opened.push(Opened {
            publisher: ProtectedRoot::open(&r.publisher_root).map_err(|_| failed())?,
            staging: ProtectedRoot::open(&r.staging_root).map_err(|_| failed())?,
            state: ProtectedRoot::open(&r.state_root).map_err(|_| failed())?,
            bootstrap: ProtectedRoot::open(&PathBuf::from(format!(
                "/var/lib/terrarium-workspace-helper/bootstrap/{}",
                r.id().map_err(|_| failed())?.0
            )))
            .map_err(|_| failed())?,
        });
    }
    let mut data = vec![&source];
    let mut controls = vec![&config_root, &socket_root];
    for r in &opened {
        data.extend([&r.publisher, &r.staging]);
        controls.push(&r.state);
        controls.push(&r.bootstrap);
    }
    // Complete read-only preflight for EVERY registration, not one at a time.
    // Even lock creation and startup quarantine snapshots must wait until the
    // state/config/socket roots and their bind aliases are known not to export.
    confinement::validate(&data, &controls).map_err(|_| failed())?;
    let mut helpers = BTreeMap::new();
    let mut bootstrap_stores = Vec::new();
    // Take every journal lock before touching the socket. A second process can
    // neither replace a live listener nor mutate this registration stream.
    for r in opened {
        let bootstrap_store = Store::open(r.bootstrap).map_err(|_| failed())?;
        let mut bootstrap = bootstrap_store.load_bootstrap().map_err(|_| failed())?;
        let binding = c.receiver.as_ref().ok_or_else(failed)?.verify_binding(bootstrap.original());
        if binding.is_err() {
            bootstrap.quarantine();
            let _ = bootstrap_store.save_bootstrap(&bootstrap);
            return Err(failed());
        }
        let identities = [&r.publisher, &r.staging, &r.state].map(|root| {
            root.fd
                .metadata()
                .map(|m| terrarium_core::live_workspace::SourceIdentity {
                    device: m.dev(),
                    inode: m.ino(),
                })
        });
        let store = Store::open(r.state).map_err(|_| failed())?;
        let journal = store.load().map_err(|_| failed())?;
        let ownership = bootstrap
            .runtime_ownership(&journal)
            .map_err(|_| failed())?;
        for (observed, expected) in
            identities
                .into_iter()
                .zip([&ownership.publisher, &ownership.staging, &ownership.state])
        {
            if &observed? != expected {
                return Err(failed());
            }
        }
        bootstrap_stores.push(bootstrap_store);
        let registration = journal.runtime.registration;
        let mut docker = runtime::DockerInspector;
        let pin =
            runtime::PinnedRuntime::acquire(&mut docker, &journal.runtime).map_err(|_| failed())?;
        let kernel = kernel::Kernel::new(
            source.try_clone().map_err(|_| failed())?,
            r.publisher,
            r.staging,
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
    let mut configuration_fenced = false;
    loop {
        let (stream, _) = listener.accept().await?;
        // Admission remains serialized per registration. No task can outlive
        // the held journal/namespace FDs or race another operation on a target.
        let result = serve_frame(stream, c.host_agent_uid, |bytes| {
            // A protected configuration edit, removal or read failure is a
            // sticky fence, not a new command/profile for an old operation.
            if configuration_fenced || !config().is_ok_and(|(current, _, _)| current == c) {
                configuration_fenced = true;
                for store in &bootstrap_stores {
                    if let Ok(mut journal) = store.load_bootstrap() {
                        journal.quarantine();
                        let _ = store.save_bootstrap(&journal);
                    }
                }
                for helper in helpers.values_mut() {
                    let _ = helper.fence();
                }
                return Err(Reply::Quarantined);
            }
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
    fn operational_paths_are_fixed_same_identity_children() {
        let id = "00000000000000000000000001";
        let mut value = serde_json::json!({"host_agent_uid":1000,"host_agent_gid":1000,"work_root":"/work","registrations":[{
            "publisher_root": format!("/var/lib/terrarium-workspace-helper/publishers/{id}"),
            "staging_root": format!("/var/lib/terrarium-workspace-helper/staging/{id}"),
            "state_root": format!("/var/lib/terrarium-workspace-helper/registrations/{id}")}]});
        // Operational registrations also require a protected specification.
        assert!(parse_config(value.to_string().as_bytes()).is_err());
        value["receiver"] = serde_json::json!({"image": format!("synthetic@sha256:{}", "a".repeat(64)), "command": ["/bin/sleep", "infinity"], "profile": {
            "kernel":"synthetic", "engine":"synthetic", "runc":"synthetic", "seccomp_digest":"synthetic", "rootful":true, "same_mount_namespace":true, "propagation_proven":true, "userns_remap":false, "namespace_creation_blocked":true}});
        assert!(parse_config(value.to_string().as_bytes()).is_ok());
        for role in ["publisher_root", "staging_root", "state_root"] {
            let original = value["registrations"][0][role].clone();
            for path in [
                "/admin/arbitrary",
                "/var/lib/terrarium-workspace-helper/publishers/00000000000000000000000002",
                "/var/lib/terrarium-workspace-helper/registrations/../registrations/00000000000000000000000001",
            ] {
                value["registrations"][0][role] = path.into();
                assert!(parse_config(value.to_string().as_bytes()).is_err());
            }
            value["registrations"][0][role] = original;
        }
    }
    #[test]
    fn config_is_bounded_strict_and_does_not_activate() {
        assert!(!capability().ready);
        assert!(!capability().compatible());
        assert!(parse_config(br#"{"host_agent_uid":1000,"host_agent_gid":1000,"work_root":"/work","registrations":[]}"#).is_ok());
        assert!(parse_config(br#"{"host_agent_uid":0,"host_agent_gid":1000,"work_root":"/work","registrations":[]}"#).is_err());
        assert!(parse_config(br#"{"host_agent_uid":1000,"host_agent_gid":1000,"work_root":"relative","registrations":[]}"#).is_err());
        assert!(parse_config(br#"{"host_agent_uid":1000,"host_agent_gid":1000,"work_root":"/work","registrations":[],"flags":1}"#).is_err());
        assert!(parse_config(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
        for root in ["/", "/run", "/etc/terrarium-workspace-helper/work"] {
            let value = serde_json::json!({"host_agent_uid":1000,"host_agent_gid":1000,"work_root":root,"registrations":[]});
            assert!(parse_config(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        for role in ["publisher_root", "staging_root", "state_root"] {
            let mut value = serde_json::json!({"host_agent_uid":1000,"host_agent_gid":1000,"work_root":"/work","registrations":[{"publisher_root":"/admin/publisher","staging_root":"/admin/staging","state_root":"/admin/state"}]});
            value["registrations"][0][role] = "/work/project/agent".into();
            assert!(parse_config(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }
}
