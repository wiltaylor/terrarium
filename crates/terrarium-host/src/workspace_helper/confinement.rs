//! Read-only preflight, before ANY store lock creation or helper snapshot.
//! Both namespace paths and filesystem-relative mount roots are compared:
//! pathname ancestry alone cannot detect bind aliases of a subtree.
use super::*;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

type Identity = (u64, u64);
fn identity(fd: &File) -> Result<Identity, Reply> {
    let m = fd.metadata().map_err(|_| Reply::Failed)?;
    Ok((m.dev(), m.ino()))
}

struct Location {
    identity: Identity,
    path: PathBuf,
    filesystem_path: PathBuf,
}
fn location(fd: &File, view: &[MountEvidence]) -> Result<Location, Reply> {
    Ok(Location {
        identity: identity(fd)?,
        path: kernel::path(fd)?,
        filesystem_path: PathBuf::from(kernel::root_of(fd, view)?.0),
    })
}
fn overlap(a: &Location, b: &Location) -> bool {
    a.identity == b.identity
        || a.path.starts_with(&b.path)
        || b.path.starts_with(&a.path)
        || (a.identity.0 == b.identity.0
            && (a.filesystem_path.starts_with(&b.filesystem_path)
                || b.filesystem_path.starts_with(&a.filesystem_path)))
}

/// Controls include the entire config/socket/state directories, so their
/// config, journal, lock, temporary snapshot and socket files are confined too.
/// Existing hard links of config/journal/lock files are separately refused by
/// their single-link file guards. No mount or filesystem mutation occurs here.
pub(super) fn validate(data: &[&ProtectedRoot], controls: &[&ProtectedRoot]) -> Result<(), Reply> {
    let view = kernel::host_view()?;
    let data_locations = data
        .iter()
        .map(|r| location(&r.fd, &view))
        .collect::<Result<Vec<_>, _>>()?;
    let control_locations = controls
        .iter()
        .map(|r| location(&r.fd, &view))
        .collect::<Result<Vec<_>, _>>()?;
    for (index, tree) in data.iter().enumerate() {
        let tree_location = &data_locations[index];
        for other in &data_locations[index + 1..] {
            if overlap(tree_location, other) {
                return Err(Reply::Unauthorized);
            }
        }
        for (control, control_location) in controls.iter().zip(&control_locations) {
            if overlap(tree_location, control_location)
                || control
                    .ancestors
                    .iter()
                    .map(identity)
                    .collect::<Result<Vec<_>, _>>()?
                    .contains(&tree_location.identity)
                || tree
                    .ancestors
                    .iter()
                    .map(identity)
                    .collect::<Result<Vec<_>, _>>()?
                    .contains(&control_location.identity)
            {
                return Err(Reply::Unauthorized);
            }
        }
        // A control directory/file may be bound into a descendant workspace,
        // not just aliased at a configured tree root. Inspect every mounted
        // descendant before allowing any store or helper to mutate state.
        for mount in &view {
            let Ok(relative) =
                std::path::Path::new(&mount.mountpoint).strip_prefix(&tree_location.path)
            else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }
            use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2};
            let fd = File::from(
                openat2(
                    &tree.fd,
                    relative,
                    OpenHow::new()
                        .flags(OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
                        .resolve(
                            ResolveFlag::RESOLVE_BENEATH
                                | ResolveFlag::RESOLVE_NO_SYMLINKS
                                | ResolveFlag::RESOLVE_NO_MAGICLINKS,
                        ),
                )
                .map_err(|_| Reply::Unauthorized)?,
            );
            if mount_id(&fd)? != mount.mount_id {
                return Err(Reply::Unauthorized);
            }
            let child = location(&fd, &view)?;
            if control_locations
                .iter()
                .any(|control| overlap(&child, control))
            {
                return Err(Reply::Unauthorized);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controls_cannot_be_equal_below_or_above_exported_trees() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["data", "state", "data/nested", "state/publisher"] {
            std::fs::create_dir(temp.path().join(name)).unwrap();
        }
        let data = ProtectedRoot::fixture(&temp.path().join("data"));
        let state = ProtectedRoot::fixture(&temp.path().join("state"));
        assert_eq!(validate(&[&data], &[&state]), Ok(()));
        assert_eq!(validate(&[&data], &[&data]), Err(Reply::Unauthorized));
        let below = ProtectedRoot::fixture(&temp.path().join("data/nested"));
        assert_eq!(validate(&[&data], &[&below]), Err(Reply::Unauthorized));
        let publisher = ProtectedRoot::fixture(&temp.path().join("state/publisher"));
        assert_eq!(validate(&[&publisher], &[&state]), Err(Reply::Unauthorized));
        // All roots share a filesystem ancestor. That alone must not reject
        // separated roots: compare each root to the other's ancestor chain,
        // never every ancestor against every ancestor.
        assert!(!data.ancestors.is_empty());
    }
}
