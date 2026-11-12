//! Strict, bounded namespace-local evidence parsing. Numeric mount IDs are
//! never compared across namespaces. A saved record is not fresh evidence.
use std::collections::BTreeSet;
use terrarium_core::live_workspace::{MountEvidence, NamespaceIdentity, Reply};

const MAX_MOUNTINFO: usize = 4 * 1024 * 1024;

fn unescape(value: &str) -> Result<String, Reply> {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let code = bytes.get(i + 1..i + 4).ok_or(Reply::Failed)?;
            out.push(match code {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return Err(Reply::Failed),
            });
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Reply::Failed)
}

fn number(s: &str) -> Result<u64, Reply> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Reply::Failed);
    }
    s.parse().map_err(|_| Reply::Failed)
}

fn flags(s: &str) -> Result<Vec<String>, Reply> {
    let values: Vec<_> = s.split(',').map(str::to_owned).collect();
    let unique: BTreeSet<_> = values.iter().collect();
    if unique.len() != values.len() || values.iter().any(String::is_empty) {
        return Err(Reply::Failed);
    }
    Ok(values)
}

pub fn parse(bytes: &[u8], namespace: NamespaceIdentity) -> Result<Vec<MountEvidence>, Reply> {
    if bytes.is_empty() || bytes.len() > MAX_MOUNTINFO || namespace.inode == 0 {
        return Err(Reply::Failed);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Reply::Failed)?;
    let mut out = Vec::new();
    let mut ids = BTreeSet::new();
    for line in text.lines() {
        if line.len() > 16384 {
            return Err(Reply::Failed);
        }
        let parts: Vec<_> = line.split(' ').collect();
        let dash = parts.iter().position(|s| *s == "-").ok_or(Reply::Failed)?;
        if dash < 6 || parts.len() != dash + 4 || parts.contains(&"") {
            return Err(Reply::Failed);
        }
        let mount_id = number(parts[0])?;
        if mount_id == 0 || !ids.insert(mount_id) {
            return Err(Reply::Failed);
        }
        let device: Vec<_> = parts[2].split(':').collect();
        if device.len() != 2 {
            return Err(Reply::Failed);
        }
        number(device[0])?;
        number(device[1])?;
        let root = unescape(parts[3])?;
        let mountpoint = unescape(parts[4])?;
        if !root.starts_with('/') || !mountpoint.starts_with('/') {
            return Err(Reply::Failed);
        }
        let optional_fields: Vec<String> = parts[6..dash].iter().map(|s| s.to_string()).collect();
        let mut types = BTreeSet::new();
        for field in &optional_fields {
            if field == "unbindable" {
                if !types.insert("unbindable") {
                    return Err(Reply::Failed);
                }
            } else {
                let (key, value) = field.split_once(':').ok_or(Reply::Failed)?;
                if !matches!(key, "shared" | "master" | "propagate_from")
                    || !types.insert(key)
                    || number(value)? == 0
                {
                    return Err(Reply::Failed);
                }
            }
        }
        out.push(MountEvidence {
            namespace: namespace.clone(),
            mount_id,
            parent_id: number(parts[1])?,
            major_minor: parts[2].into(),
            root,
            mountpoint,
            vfs_flags: flags(parts[5])?,
            super_flags: flags(parts[dash + 3])?,
            optional_fields,
        });
    }
    Ok(out)
}

fn has(flags: &[String], value: &str) -> bool {
    flags.iter().any(|v| v == value)
}
fn group(m: &MountEvidence, name: &str) -> Option<u64> {
    m.optional_fields
        .iter()
        .find_map(|f| f.strip_prefix(name).and_then(|v| v.parse().ok()))
}

/// Detached clone must have undergone an explicit private transition before
/// publication. nosuid/nodev apply to the VFS mount, not merely super options.
pub fn private_stage(m: &MountEvidence, root: &str, device: &str) -> Result<(), Reply> {
    if m.root != root
        || m.major_minor != device
        || !m.optional_fields.is_empty()
        || !has(&m.vfs_flags, "rw")
        || has(&m.vfs_flags, "ro")
        || !has(&m.vfs_flags, "nosuid")
        || !has(&m.vfs_flags, "nodev")
        || has(&m.vfs_flags, "noexec")
        || has(&m.super_flags, "ro")
    {
        return Err(Reply::Quarantined);
    }
    Ok(())
}

/// Verify one host publisher and its distinct-namespace read-only receiver.
/// The caller also checks the complete child allowlist and pinned runtime.
pub fn publisher(
    host: &MountEvidence,
    receiver: &MountEvidence,
    host_view: &[MountEvidence],
) -> Result<u64, Reply> {
    publisher_at(host, receiver, host_view, "/work")
}

fn publisher_at(host: &MountEvidence, receiver: &MountEvidence, host_view: &[MountEvidence], receiver_path: &str) -> Result<u64, Reply> {
    let shared = group(host, "shared:").ok_or(Reply::Quarantined)?;
    if host.namespace == receiver.namespace
        || host.root != receiver.root
        || host.major_minor != receiver.major_minor
        || receiver.mountpoint != receiver_path
        || host.optional_fields != vec![format!("shared:{shared}")]
        || receiver.optional_fields != vec![format!("master:{shared}")]
        || !has(&receiver.vfs_flags, "ro")
        || has(&receiver.vfs_flags, "rw")
        || !has(&host.vfs_flags, "rw")
        || has(&host.vfs_flags, "ro")
        || host_view
            .iter()
            .filter(|m| m.namespace == host.namespace && group(m, "shared:") == Some(shared))
            .count()
            != 1
    {
        return Err(Reply::Quarantined);
    }
    Ok(shared)
}

#[cfg(test)]
pub(super) fn publisher_fixture(host: &MountEvidence, receiver: &MountEvidence, host_view: &[MountEvidence], path: &str) -> Result<u64, Reply> {
    publisher_at(host, receiver, host_view, path)
}

/// No extra child mounts or aliases within the protected publisher subtree.
pub fn exact_children(
    view: &[MountEvidence],
    parent: &MountEvidence,
    allowed: &[String],
) -> Result<(), Reply> {
    let prefix = format!("{}/", parent.mountpoint.trim_end_matches('/'));
    let children: Vec<_> = view
        .iter()
        .filter(|m| m.namespace == parent.namespace && m.mountpoint.starts_with(&prefix))
        .collect();
    let actual: BTreeSet<_> = children.iter().map(|m| &m.mountpoint).collect();
    let expected: BTreeSet<_> = allowed.iter().collect();
    if actual != expected
        || children.len() != allowed.len()
        || children.iter().any(|m| m.parent_id != parent.mount_id)
    {
        return Err(Reply::Quarantined);
    }
    Ok(())
}

/// Verify a propagated writable submount against the enrolled source and both
/// publisher views. Host and receiver parent IDs are checked locally only.
pub fn workspace(
    host: &MountEvidence,
    receiver: &MountEvidence,
    host_publisher: &MountEvidence,
    receiver_publisher: &MountEvidence,
    root: &str,
    device: &str,
) -> Result<(), Reply> {
    let shared = group(host, "shared:").ok_or(Reply::Quarantined)?;
    if host.namespace != host_publisher.namespace
        || receiver.namespace != receiver_publisher.namespace
        || host.namespace == receiver.namespace
        || host.parent_id != host_publisher.mount_id
        || receiver.parent_id != receiver_publisher.mount_id
        || host.mount_id == host.parent_id
        || receiver.mount_id == receiver.parent_id
        || host.optional_fields != vec![format!("shared:{shared}")]
        || receiver.optional_fields != vec![format!("master:{shared}")]
    {
        return Err(Reply::Quarantined);
    }
    for mount in [host, receiver] {
        if mount.root != root
            || mount.major_minor != device
            || !has(&mount.vfs_flags, "rw")
            || has(&mount.vfs_flags, "ro")
            || !has(&mount.vfs_flags, "nosuid")
            || !has(&mount.vfs_flags, "nodev")
            || has(&mount.vfs_flags, "noexec")
            || has(&mount.super_flags, "ro")
        {
            return Err(Reply::Quarantined);
        }
    }
    let host_suffix = host
        .mountpoint
        .strip_prefix(&format!("{}/", host_publisher.mountpoint))
        .ok_or(Reply::Quarantined)?;
    let receiver_prefix = format!("{}/", receiver_publisher.mountpoint);
    let receiver_suffix = receiver
        .mountpoint
        .strip_prefix(&receiver_prefix)
        .ok_or(Reply::Quarantined)?;
    if host_suffix != receiver_suffix || host_suffix.split('/').count() != 2 {
        return Err(Reply::Quarantined);
    }
    Ok(())
}
