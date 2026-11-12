//! Construct an empty helper-owned mount scaffold. Not wired to a service
//! route. Invoking this requires the separately approved privileged window.
use super::mounts::{self, Attributes};
use super::registration_roots::Directories;
use super::{ProtectedRoot, Store, kernel, mount_id, mountinfo, paths};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use terrarium_core::live_workspace::bootstrap::Scaffold;
use terrarium_core::live_workspace::{MountEvidence, RegistrationId, Reply, SourceIdentity};

/// Retains every administrative FD until receiver verification and commit.
/// This is empty host-scaffold evidence only, never attachment evidence.
pub struct Mounted {
    directories: Directories,
    evidence: Scaffold,
}

impl Mounted {
    pub fn evidence(&self) -> &Scaffold {
        &self.evidence
    }

    pub fn into_directories(self) -> Directories {
        self.directories
    }

    pub fn publisher(&self) -> &ProtectedRoot {
        &self.directories.publisher
    }
}

fn source(root: &ProtectedRoot) -> Result<SourceIdentity, Reply> {
    let m = root.fd.metadata().map_err(|_| Reply::Failed)?;
    if m.uid() != 0 || !m.is_dir() || m.mode() & 0o077 != 0 {
        return Err(Reply::Unauthorized);
    }
    Ok(SourceIdentity {
        device: m.dev(),
        inode: m.ino(),
    })
}

fn private_surrounding(record: &MountEvidence) -> Result<(), Reply> {
    // A clone must not be attached beneath an inherited shared/slave parent:
    // that first attachment could otherwise propagate before isolation.
    if !record.optional_fields.is_empty() {
        return Err(Reply::Unsupported);
    }
    Ok(())
}

fn isolate(root: &mut ProtectedRoot, id: RegistrationId, shared: bool) -> Result<(), Reply> {
    paths::empty(&root.fd)?;
    let parent = root.ancestors.last().ok_or(Reply::Quarantined)?;
    let old_id = mount_id(&root.fd)?;
    if old_id != mount_id(parent)? {
        return Err(Reply::Quarantined);
    }
    let before = kernel::host_view()?;
    let surrounding = before
        .iter()
        .find(|m| m.mount_id == old_id)
        .ok_or(Reply::Quarantined)?;
    private_surrounding(surrounding)?;
    let (expected_root, device) = kernel::root_of(&root.fd, &before)?;
    let tree = mounts::clone_nonrecursive(&root.fd)?;
    mounts::attach(&tree, &root.fd)?;
    *root = root.reopen_registration_child(id)?;
    if mount_id(&root.fd)? == old_id {
        return Err(Reply::Quarantined);
    }
    mounts::make_private(&root.fd)?;
    mounts::LinuxAttributes.scaffold(&root.fd)?;
    let view = kernel::host_view()?;
    let current_id = mount_id(&root.fd)?;
    let record = view
        .iter()
        .find(|m| m.mount_id == current_id)
        .ok_or(Reply::Quarantined)?;
    mountinfo::private_stage(record, &expected_root, &device)?;
    mountinfo::exact_children(&view, record, &[])?;
    if shared {
        mounts::make_shared(&root.fd)?;
        let view = kernel::host_view()?;
        let record = view
            .iter()
            .find(|m| m.mount_id == current_id)
            .ok_or(Reply::Quarantined)?;
        let [group] = record.optional_fields.as_slice() else {
            return Err(Reply::Quarantined);
        };
        if !group
            .strip_prefix("shared:")
            .is_some_and(|n| n.parse::<u64>().is_ok_and(|id| id > 0))
            || view
                .iter()
                .filter(|m| m.optional_fields.contains(group))
                .count()
                != 1
        {
            return Err(Reply::Quarantined);
        }
        // Recheck restrictions after the propagation transition too.
        let mut restrictions = record.clone();
        restrictions.optional_fields.clear();
        mountinfo::private_stage(&restrictions, &expected_root, &device)?;
        mountinfo::exact_children(&view, record, &[])?;
    }
    Ok(())
}

/// Call only from the fresh bootstrap continuation while its Store lock is
/// held. Errors retain unknown mounts/children and durably quarantine when
/// possible. There is no cleanup, adoption or permissive fallback.
pub fn build(store: &Store, mut directories: Directories) -> Result<Mounted, Reply> {
    let mut journal = store.load_bootstrap()?;
    if !journal.initial() {
        return Err(Reply::Quarantined);
    }
    let id = journal.original().registration;
    let result = (|| {
        let expected = [
            (&directories.publisher, "publishers"),
            (&directories.staging, "staging"),
            (&directories.registration, "registrations"),
        ];
        for (root, parent) in expected {
            let path = Path::new("/var/lib/terrarium-workspace-helper")
                .join(parent)
                .join(id.0.to_string());
            if kernel::path(&root.fd)? != path {
                return Err(Reply::Unauthorized);
            }
            source(root)?;
            paths::empty(&root.fd)?;
        }
        isolate(&mut directories.publisher, id, true)?;
        isolate(&mut directories.staging, id, false)?;
        let evidence = Scaffold {
            publisher: source(&directories.publisher)?,
            staging: source(&directories.staging)?,
            state: source(&directories.registration)?,
        };
        Ok(Mounted {
            directories,
            evidence,
        })
    })();
    if result.is_err() {
        journal.recover_without_evidence();
        store.save_bootstrap(&journal)?;
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn foreign_paths_quarantine_before_any_mount_action() {
        let dir = tempfile::tempdir().unwrap();
        let roots = super::super::registration_roots::Roots::fixture(dir.path());
        let request = serde_json::from_value(serde_json::json!({
            "version": 1,
            "operation": "00000000000000000000000001",
            "provenance": {"daemon_instance": "00000000000000000000000002", "caller": "local_operator"},
            "host": "00000000000000000000000003",
            "target": "synthetic",
            "registration": "00000000000000000000000004",
            "incarnation": "00000000000000000000000005",
            "domain": "00000000000000000000000006"
        })).unwrap();
        let store = roots.begin(request).unwrap();
        let directories = roots.create_directories(&store).unwrap();
        let pinned = directories.publisher.fd.try_clone().unwrap();
        let before = mount_id(&pinned).unwrap();
        assert!(matches!(
            build(&store, directories),
            Err(Reply::Unauthorized)
        ));
        assert_eq!(mount_id(&pinned).unwrap(), before);
        assert!(store.load_bootstrap().unwrap().quarantined());
    }

    #[test]
    fn parent_propagation_must_be_private_before_first_attachment() {
        let namespace = terrarium_core::live_workspace::NamespaceIdentity {
            device: 1,
            inode: 2,
        };
        for optional in ["shared:9", "master:9", "shared:9 master:8", "unbindable"] {
            let line = format!("1 2 0:1 / / rw {optional} - tmpfs tmpfs rw\n");
            let records = mountinfo::parse(line.as_bytes(), namespace.clone()).unwrap();
            assert_eq!(private_surrounding(&records[0]), Err(Reply::Unsupported));
        }
        let records = mountinfo::parse(b"1 2 0:1 / / rw - tmpfs tmpfs rw\n", namespace).unwrap();
        assert_eq!(private_surrounding(&records[0]), Ok(()));
    }
}
