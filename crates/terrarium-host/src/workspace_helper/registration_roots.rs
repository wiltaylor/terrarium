//! Helper-selected administrative roots. No arbitrary root paths are accepted
//! by the public API. This does not mount a scaffold or verify a receiver.
use super::{ProtectedRoot, Store};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use terrarium_core::live_workspace::Reply;
use terrarium_core::live_workspace::bootstrap::{Journal, Request};

pub struct Roots {
    publisher: ProtectedRoot,
    staging: ProtectedRoot,
    registration: ProtectedRoot,
    bootstrap: ProtectedRoot,
    owner: u32,
}

/// Pinned directories only, NOT verified mount-scaffold evidence.
pub struct Directories {
    pub publisher: ProtectedRoot,
    pub staging: ProtectedRoot,
    pub registration: ProtectedRoot,
}

impl Roots {
    /// Parents must already exist with protected ancestors. No repair, chmod,
    /// symlink following or operational journal import is attempted.
    pub fn open(source: &ProtectedRoot, controls: &[&ProtectedRoot]) -> Result<Self, Reply> {
        let roots = Self {
            publisher: ProtectedRoot::open(Path::new(
                "/var/lib/terrarium-workspace-helper/publishers",
            ))?,
            staging: ProtectedRoot::open(Path::new("/var/lib/terrarium-workspace-helper/staging"))?,
            registration: ProtectedRoot::open(Path::new(
                "/var/lib/terrarium-workspace-helper/registrations",
            ))?,
            bootstrap: ProtectedRoot::open(Path::new(
                "/var/lib/terrarium-workspace-helper/bootstrap",
            ))?,
            owner: 0,
        };
        roots.distinct()?;
        let mut protected = vec![&roots.registration, &roots.bootstrap];
        protected.extend_from_slice(controls);
        super::confinement::validate(&[source, &roots.publisher, &roots.staging], &protected)?;
        Ok(roots)
    }

    fn distinct(&self) -> Result<(), Reply> {
        let mut identities = Vec::new();
        for root in [
            &self.publisher,
            &self.staging,
            &self.registration,
            &self.bootstrap,
        ] {
            let m = root.fd.metadata().map_err(|_| Reply::Failed)?;
            let identity = (m.dev(), m.ino());
            if identities.contains(&identity) {
                return Err(Reply::Unauthorized);
            }
            identities.push(identity);
        }
        // Inode inequality alone does not reject ancestry or bind aliases.
        let roots = [
            &self.publisher,
            &self.staging,
            &self.registration,
            &self.bootstrap,
        ];
        for (i, root) in roots.iter().enumerate() {
            let others: Vec<_> = roots
                .iter()
                .enumerate()
                .filter_map(|(j, other)| (i != j).then_some(*other))
                .collect();
            super::confinement::validate(&[*root], &others)?;
        }
        Ok(())
    }

    /// The journal directory is metadata, not an operational scaffold. A
    /// leftover child after a crash refuses allocation, including empty ones.
    pub fn begin(&self, request: Request) -> Result<Store, Reply> {
        let journal = Journal::new(request)?;
        self.distinct()?;
        let root = self
            .bootstrap
            .create_registration_child(journal.original().registration, self.owner)?;
        let store = Store::open_owned(root, self.owner)?;
        store.begin_bootstrap(&journal)?;
        Ok(store)
    }

    /// Must precede scaffold mount construction. The caller must quarantine
    /// every error. Partial children are retained as unknown state, not deleted
    /// or adopted. This function never emits ScaffoldVerified.
    pub fn create_directories(&self, store: &Store) -> Result<Directories, Reply> {
        let journal = store.load_bootstrap()?;
        if !journal.initial() {
            return Err(Reply::Quarantined);
        }
        self.distinct()?;
        let id = journal.original().registration;
        let result = (|| {
            Ok(Directories {
                publisher: self.publisher.create_registration_child(id, self.owner)?,
                staging: self.staging.create_registration_child(id, self.owner)?,
                registration: self
                    .registration
                    .create_registration_child(id, self.owner)?,
            })
        })();
        if result.is_err() {
            let mut fenced = journal;
            fenced.recover_without_evidence();
            store.save_bootstrap(&fenced)?;
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
impl Roots {
    pub(super) fn fixture(dir: &Path) -> Self {
        let make = |name: &str| {
            let path = dir.join(name);
            std::fs::create_dir(&path).unwrap();
            ProtectedRoot::fixture(&path)
        };
        Self {
            publisher: make("publisher"),
            staging: make("staging"),
            registration: make("registration"),
            bootstrap: make("bootstrap"),
            owner: nix::unistd::geteuid().as_raw(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn request() -> Request {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "operation": "00000000000000000000000001",
            "provenance": {"daemon_instance": "00000000000000000000000002", "caller": "local_operator"},
            "host": "00000000000000000000000003",
            "target": "synthetic",
            "registration": "00000000000000000000000004",
            "incarnation": "00000000000000000000000005",
            "domain": "00000000000000000000000006"
        })).unwrap()
    }

    fn fixture() -> (tempfile::TempDir, Roots) {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::fixture(dir.path());
        (dir, roots)
    }

    #[test]
    fn exclusive_metadata_intent_precedes_all_operational_children() {
        let (dir, roots) = fixture();
        let request = request();
        let id = request.registration.0.to_string();
        let store = roots.begin(request.clone()).unwrap();
        assert!(roots.begin(request.clone()).is_err());
        assert_eq!(store.load_bootstrap().unwrap().original(), &request);
        assert!(!dir.path().join("publisher").join(&id).exists());
        let pinned = roots.create_directories(&store).unwrap();
        for root in [pinned.publisher, pinned.staging, pinned.registration] {
            assert_eq!(root.fd.metadata().unwrap().mode() & 0o777, 0o700);
        }
        assert!(roots.create_directories(&store).is_err());
        assert!(store.load_bootstrap().unwrap().quarantined());
    }

    #[test]
    fn unknown_metadata_and_partial_scaffold_are_never_adopted() {
        let (dir, roots) = fixture();
        let request = request();
        let id = request.registration.0.to_string();
        let unknown = dir.path().join("bootstrap").join(&id);
        std::fs::create_dir(&unknown).unwrap();
        assert!(roots.begin(request.clone()).is_err());
        assert!(!unknown.join("bootstrap.json").exists());

        let (dir, roots) = fixture();
        let store = roots.begin(request).unwrap();
        let foreign = dir.path().join("staging").join(&id);
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("foreign"), b"retain").unwrap();
        assert!(roots.create_directories(&store).is_err());
        assert_eq!(std::fs::read(foreign.join("foreign")).unwrap(), b"retain");
        assert!(store.load_bootstrap().unwrap().quarantined());
    }

    #[test]
    fn alias_parents_and_writable_parent_substitution_are_refused() {
        let (dir, mut roots) = fixture();
        roots.staging = roots.publisher.try_clone().unwrap();
        assert!(roots.begin(request()).is_err());
        assert_eq!(
            std::fs::read_dir(dir.path().join("bootstrap"))
                .unwrap()
                .count(),
            0
        );

        let (dir, roots) = fixture();
        std::fs::set_permissions(
            dir.path().join("bootstrap"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(roots.begin(request()).is_err());
    }

    #[test]
    fn nested_parent_is_not_mistaken_for_distinct_protected_storage() {
        let (dir, mut roots) = fixture();
        let nested = dir.path().join("publisher/nested");
        std::fs::create_dir(&nested).unwrap();
        roots.staging = ProtectedRoot::fixture(&nested);
        assert!(roots.begin(request()).is_err());
        assert_eq!(
            std::fs::read_dir(dir.path().join("bootstrap"))
                .unwrap()
                .count(),
            0
        );
    }
}
