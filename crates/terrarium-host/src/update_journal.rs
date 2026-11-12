//! Durable versioned-image transaction. Switching the service configuration
//! never moves or removes either executable.
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Journal {
    pub previous: PathBuf,
    pub next: PathBuf,
    pub version: String,
    pub confirmed: bool,
}

impl Journal {
    /// Called only by a successful welcome on a start after confirmation.
    pub fn collect_previous(&self, journal: &Path) -> io::Result<()> {
        if !self.confirmed || self.previous == self.next
            || self.previous.parent().and_then(Path::parent) != self.next.parent().and_then(Path::parent)
        { return Err(io::Error::other("invalid confirmed cleanup transaction")); }
        let directory = self.previous.parent().ok_or_else(|| io::Error::other("missing previous directory"))?;
        if directory.exists() { std::fs::remove_dir_all(directory)?; }
        std::fs::remove_file(journal)
    }
    pub fn read(path: &Path) -> io::Result<Self> {
        serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)
    }

    pub fn persist(&self, path: &Path) -> io::Result<()> {
        let temporary = path.with_extension("writing");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        // Windows rename replaces a regular file without first unlinking it.
        std::fs::rename(temporary, path)
    }

    /// The destination directory must be new. It inherits the install root's
    /// protected ACL. An interrupted copy cannot be selected by SCM.
    pub fn stage(source: &Path, previous: &Path, root: &Path, version: &str) -> io::Result<Self> {
        if version.is_empty()
            || !version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".+_-".contains(&b))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid version directory",
            ));
        }
        if !previous.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "previous executable missing",
            ));
        }
        let directory = root.join(version);
        std::fs::create_dir(&directory)?;
        let next = directory.join("terrarium-host.exe");
        std::fs::copy(source, &next)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&next)?
            .sync_all()?;
        if std::fs::read(source)? != std::fs::read(&next)? {
            return Err(io::Error::other(
                "staged executable differs from verified source",
            ));
        }
        Ok(Self {
            previous: previous.to_owned(),
            next,
            version: version.to_owned(),
            confirmed: false,
        })
    }

    /// The callback performs exactly one atomic ImagePath switch. Persist the
    /// recovery record first. A failure leaves the previous image untouched.
    pub fn activate(
        &self,
        path: &Path,
        switch: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        if !self.previous.is_file() || !self.next.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "transaction image missing",
            ));
        }
        self.persist(path)?;
        switch(&self.next)
    }

    /// Keep the journal across rollback. A crash immediately after the SCM
    /// switch is distinguishable on restart by its configured image path.
    pub fn rollback(&self, switch: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
        if !self.previous.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "rollback image missing",
            ));
        }
        switch(&self.previous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_start_without_welcome_preserves_rollback_image() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::create_dir(root.join("old"))?;
        let previous = root.join("old/terrarium-host.exe");
        std::fs::write(&previous, b"verified previous image")?;
        let mut transaction = Journal::stage(&previous, &previous, root, "new")?;
        let journal = root.join("pending.json");
        transaction.confirmed = true;
        transaction.persist(&journal)?;
        // Later startup only reads the journal. Config/connection failure or
        // process death before welcome must leave both images intact.
        let later_start = Journal::read(&journal)?;
        drop(later_start);
        assert!(previous.is_file());
        assert!(transaction.next.is_file());
        assert!(journal.is_file());
        // The subsequent successful welcome may collect the previous image.
        Journal::read(&journal)?.collect_previous(&journal)?;
        assert!(!previous.exists());
        assert!(transaction.next.is_file());
        assert!(!journal.exists());
        Ok(())
    }

    #[test]
    fn persisted_boundaries_always_select_an_existing_image() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let previous = root.join("previous.exe");
        let source = root.join("verified.exe");
        let journal = root.join("pending.json");
        std::fs::write(&previous, b"previous verified image")?;
        std::fs::write(&source, b"next verified image")?;
        let transaction = Journal::stage(&source, &previous, root, "1.0.1+test")?;
        // Power loss after writing the version directory: SCM is unchanged.
        assert!(previous.is_file());
        assert!(!journal.exists());
        // Power loss after the journal but before the switch.
        transaction.persist(&journal)?;
        assert_eq!(Journal::read(&journal)?, transaction);
        assert!(previous.is_file());
        let mut configured = previous.clone();
        transaction.activate(&journal, |next| {
            configured = next.to_owned();
            Ok(())
        })?;
        // After the switch and after process exit, the same path still exists.
        assert_eq!(configured, transaction.next);
        assert!(configured.is_file());
        let recovered = Journal::read(&journal)?;
        recovered.rollback(|old| {
            configured = old.to_owned();
            Ok(())
        })?;
        // Power loss during rollback is safe even before journal cleanup.
        assert_eq!(configured, previous);
        assert!(configured.is_file());
        assert_eq!(Journal::read(&journal)?, transaction);
        recovered.rollback(|old| {
            assert!(old.is_file());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn switch_failure_preserves_previous_and_journal() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let previous = root.join("previous.exe");
        std::fs::write(&previous, b"verified")?;
        let transaction = Journal::stage(&previous, &previous, root, "next")?;
        let path = root.join("pending.json");
        assert!(
            transaction
                .activate(&path, |_| Err(io::Error::other("SCM denied")))
                .is_err()
        );
        assert!(previous.is_file());
        assert_eq!(Journal::read(&path)?, transaction);
        assert!(Journal::stage(&previous, &previous, root, "../escape").is_err());
        Ok(())
    }
}
