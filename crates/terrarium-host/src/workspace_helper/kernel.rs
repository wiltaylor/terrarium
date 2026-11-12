//! The isolated root backend. Construction requires already provisioned, empty
//! per-registration publisher/private-staging mounts and a pinned runtime.
//! B3 supplies the create-only scaffold. No production flow constructs this.
use super::mounts::{Attributes, Unmount};
use super::runtime::{Inspector, PinnedRuntime};
use super::*;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use terrarium_core::live_workspace::{
    AttachmentId, Enrollment, Holder, NamespaceIdentity, WorkspaceId,
};

struct Pending {
    envelope: Envelope,
    source: File,
    staged: File,
    stage: Option<Destination>,
    destination: Destination,
    root: String,
    device: String,
}

pub struct Kernel<I, A> {
    source: ProtectedRoot,
    publisher: ProtectedRoot,
    staging: ProtectedRoot,
    runtime: PinnedRuntime,
    inspector: I,
    attributes: A,
    live: BTreeMap<AttachmentId, Holder>,
    known: BTreeMap<WorkspaceId, Holder>,
    pending: Option<Pending>,
    revoking: Option<(Holder, File)>,
    receiver_relative: String,
}

fn path(fd: &File) -> Result<PathBuf, Reply> {
    let p = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .map_err(|_| Reply::Quarantined)?;
    if !p.is_absolute() || p.to_string_lossy().ends_with(" (deleted)") {
        return Err(Reply::Quarantined);
    }
    Ok(p)
}
fn host_view() -> Result<Vec<MountEvidence>, Reply> {
    let mut bytes = Vec::new();
    File::open("/proc/self/mountinfo")
        .map_err(|_| Reply::Failed)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Reply::Failed)?;
    let ns = std::fs::metadata("/proc/self/ns/mnt").map_err(|_| Reply::Failed)?;
    mountinfo::parse(
        &bytes,
        NamespaceIdentity {
            device: ns.dev(),
            inode: ns.ino(),
        },
    )
}
fn at<'a>(view: &'a [MountEvidence], path: &Path) -> Result<&'a MountEvidence, Reply> {
    let mut matches = view.iter().filter(|m| Path::new(&m.mountpoint) == path);
    let m = matches.next().ok_or(Reply::Quarantined)?;
    if matches.next().is_some() {
        return Err(Reply::Quarantined);
    }
    Ok(m)
}
fn same_source(fd: &File, e: &Enrollment) -> Result<(), Reply> {
    let m = fd.metadata().map_err(|_| Reply::Quarantined)?;
    if !m.is_dir() || (m.dev(), m.ino()) != (e.source.device, e.source.inode) {
        return Err(Reply::Quarantined);
    }
    Ok(())
}
fn root_of(source: &File, view: &[MountEvidence]) -> Result<(String, String), Reply> {
    let id = mount_id(source)?;
    let mount = view
        .iter()
        .find(|m| m.mount_id == id)
        .ok_or(Reply::Quarantined)?;
    let full = path(source)?;
    let relative = full
        .strip_prefix(&mount.mountpoint)
        .map_err(|_| Reply::Quarantined)?;
    let root = Path::new(&mount.root)
        .join(relative)
        .to_str()
        .ok_or(Reply::Quarantined)?
        .to_owned();
    Ok((root, mount.major_minor.clone()))
}
fn open_receiver(root: &File, relative: &str) -> Result<File, Reply> {
    use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2};
    openat2(
        root,
        relative,
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            ),
    )
    .map(File::from)
    .map_err(|_| Reply::Quarantined)
}
fn absent(root: &File, relative: &str) -> Result<(), Reply> {
    use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2};
    match openat2(
        root,
        relative,
        OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            ),
    ) {
        Err(nix::errno::Errno::ENOENT) => Ok(()),
        _ => Err(Reply::Quarantined),
    }
}

impl<I: Inspector, A: Attributes> Kernel<I, A> {
    pub fn new(
        source: ProtectedRoot,
        publisher: ProtectedRoot,
        staging: ProtectedRoot,
        runtime: PinnedRuntime,
        inspector: I,
        attributes: A,
    ) -> Result<Self, Reply> {
        if !nix::unistd::geteuid().is_root() {
            return Err(Reply::Unauthorized);
        }
        let source_id = mount_id(&source.fd)?;
        let publisher_id = mount_id(&publisher.fd)?;
        let staging_id = mount_id(&staging.fd)?;
        if source_id == publisher_id || source_id == staging_id || publisher_id == staging_id {
            return Err(Reply::Quarantined);
        }
        let roots = [path(&source.fd)?, path(&publisher.fd)?, path(&staging.fd)?];
        for (n, a) in roots.iter().enumerate() {
            for b in &roots[n + 1..] {
                if a.starts_with(b) || b.starts_with(a) {
                    return Err(Reply::Quarantined);
                }
            }
        }
        Ok(Self {
            source,
            publisher,
            staging,
            runtime,
            inspector,
            attributes,
            live: BTreeMap::new(),
            known: BTreeMap::new(),
            pending: None,
            revoking: None,
            receiver_relative: "work".into(),
        })
    }

    #[cfg(test)]
    pub(super) fn fixture_receiver(mut self, path: &Path) -> Self {
        self.receiver_relative = path.strip_prefix("/").unwrap().to_str().unwrap().to_owned();
        self
    }

    fn source_root(&self, e: &Enrollment) -> Result<(File, String, String), Reply> {
        let fd = self.source.enrolled(e)?;
        let (root, device) = root_of(&fd, &host_view()?)?;
        Ok((fd, root, device))
    }

    fn remember(&mut self, holder: &Holder) -> Result<(), Reply> {
        let e = &holder.enrollment;
        if let Some(old) = self.known.get(&e.workspace) {
            if old.enrollment != *e || old.domain != holder.domain {
                return Err(Reply::Conflict);
            }
        } else {
            if self.known.values().any(|h| {
                h.enrollment.source == e.source
                    || (h.enrollment.project == e.project && h.enrollment.name == e.name)
            }) {
                return Err(Reply::Conflict);
            }
            self.known.insert(e.workspace, holder.clone());
        }
        Ok(())
    }

    fn topology(&mut self) -> Result<(Vec<MountEvidence>, Vec<MountEvidence>), Reply> {
        self.runtime
            .recheck(&mut self.inspector, self.runtime.identity())?;
        let host = host_view()?;
        let receiver = self.runtime.mountinfo(&mut self.inspector)?;
        let publisher_path = path(&self.publisher.fd)?;
        let hp = at(&host, &publisher_path)?;
        if mount_id(&self.publisher.fd)? != hp.mount_id {
            return Err(Reply::Quarantined);
        }
        let receiver_path = PathBuf::from("/").join(&self.receiver_relative);
        let rp = at(&receiver, &receiver_path)?;
        mountinfo::publisher_at(
            hp,
            rp,
            &host,
            receiver_path.to_str().ok_or(Reply::Quarantined)?,
        )?;
        let receiver_fd = open_receiver(&self.runtime.root, &self.receiver_relative)?;
        if mount_id(&receiver_fd)? != rp.mount_id {
            return Err(Reply::Quarantined);
        }
        let staging_path = path(&self.staging.fd)?;
        let stage = at(&host, &staging_path)?;
        if !stage.optional_fields.is_empty() || mount_id(&self.staging.fd)? != stage.mount_id {
            return Err(Reply::Quarantined);
        }
        let stage_allowed = self
            .pending
            .as_ref()
            .filter(|p| p.stage.is_some())
            .map(|p| path(&p.staged).map(|v| vec![v.to_string_lossy().into_owned()]))
            .transpose()?
            .unwrap_or_default();
        mountinfo::exact_children(&host, stage, &stage_allowed)?;
        let mut host_allowed = vec![];
        let mut receiver_allowed = vec![];
        for holder in self.live.values() {
            let e = &holder.enrollment;
            let (_source, root, device) = self.source_root(e)?;
            let hp_path = publisher_path.join(&e.project).join(&e.name);
            let rp_path = receiver_path.join(&e.project).join(&e.name);
            let hm = at(&host, &hp_path)?;
            let rm = at(&receiver, &rp_path)?;
            mountinfo::workspace(hm, rm, hp, rp, &root, &device)?;
            let opened_host = self.publisher.workspace_mount(e)?;
            let opened_receiver = open_receiver(
                &self.runtime.root,
                &format!("{}/{}/{}", self.receiver_relative, e.project, e.name),
            )?;
            same_source(&opened_host, e)?;
            same_source(&opened_receiver, e)?;
            if mount_id(&opened_host)? != hm.mount_id || mount_id(&opened_receiver)? != rm.mount_id
            {
                return Err(Reply::Quarantined);
            }
            host_allowed.push(hp_path.to_string_lossy().into_owned());
            receiver_allowed.push(rp_path.to_string_lossy().into_owned());
        }
        mountinfo::exact_children(&host, hp, &host_allowed)?;
        mountinfo::exact_children(&receiver, rp, &receiver_allowed)?;
        self.runtime
            .recheck(&mut self.inspector, self.runtime.identity())?;
        // Save only registration-local records, not unrelated host paths.
        let host_evidence = host
            .into_iter()
            .filter(|m| {
                m.mountpoint == publisher_path.to_string_lossy()
                    || host_allowed.contains(&m.mountpoint)
            })
            .collect();
        let receiver_evidence = receiver
            .into_iter()
            .filter(|m| {
                m.mountpoint == receiver_path.to_string_lossy()
                    || receiver_allowed.contains(&m.mountpoint)
            })
            .collect();
        Ok((host_evidence, receiver_evidence))
    }
}

impl<I: Inspector, A: Attributes> Backend for Kernel<I, A> {
    fn reconcile(&mut self, journal: &Journal) -> Result<(), Reply> {
        if journal.runtime != *self.runtime.identity() {
            return Err(Reply::Quarantined);
        }
        let mut history: Vec<_> = journal
            .operations
            .values()
            .filter(|i| i.result.is_some())
            .collect();
        history.sort_by_key(|i| i.envelope.original.revision);
        self.live.clear();
        self.known.clear();
        for i in history {
            let e = &i.envelope;
            if let Some(holder) = &e.original.holder {
                self.remember(holder)?;
            }
            if e.delivery == Delivery::Superseded {
                continue;
            }
            if let Some(holder) = &e.original.holder {
                match e.original.kind {
                    Kind::Grant => {
                        if self.live.values().any(|h| {
                            h.enrollment.workspace == holder.enrollment.workspace
                                || (h.enrollment.project == holder.enrollment.project
                                    && h.enrollment.name == holder.enrollment.name)
                        }) {
                            return Err(Reply::Quarantined);
                        }
                        if self
                            .live
                            .insert(holder.attachment, holder.clone())
                            .is_some()
                        {
                            return Err(Reply::Quarantined);
                        }
                    }
                    Kind::Revoke if self.live.remove(&holder.attachment).as_ref() != Some(holder) => return Err(Reply::Quarantined),
                    _ => {}
                }
            }
        }
        let (host, receiver) = self.topology()?;
        // A clean helper restart must observe the exact last committed topology,
        // not merely mounts that happen to have similar paths/source strings.
        if let Some(last) = journal
            .operations
            .values()
            .filter(|i| i.result.is_some() && i.envelope.delivery == Delivery::Apply)
            .max_by_key(|i| i.envelope.original.revision)
            && (last.host_evidence != host || last.receiver_evidence != receiver)
        {
            return Err(Reply::Quarantined);
        }
        Ok(())
    }

    fn check_runtime(&mut self, e: &Envelope) -> Result<(), Reply> {
        if e.original.runtime != *self.runtime.identity() {
            return Err(Reply::Quarantined);
        }
        self.topology().map(|_| ())
    }

    fn stage(&mut self, e: &Envelope) -> Result<(), Reply> {
        if self.pending.is_some() || self.revoking.is_some() {
            return Err(Reply::Busy);
        }
        let holder = e.original.holder.as_ref().ok_or(Reply::Failed)?;
        self.remember(holder)?;
        if self.live.contains_key(&holder.attachment)
            || self.live.values().any(|h| {
                h.enrollment.workspace == holder.enrollment.workspace
                    || (h.enrollment.project == holder.enrollment.project
                        && h.enrollment.name == holder.enrollment.name)
            })
        {
            return Err(Reply::Conflict);
        }
        let (source, root, device) = self.source_root(&holder.enrollment)?;
        scan_source(&source)?;
        let stage = self.staging.destination(&holder.enrollment)?;
        let destination = self.publisher.destination(&holder.enrollment)?;
        let tree = mounts::clone_nonrecursive(&source)?;
        mounts::attach(&tree, stage.placeholder()?)?;
        let staged = stage.mounted()?;
        let view = host_view()?;
        let observed = at(&view, &path(&staged)?)?;
        if observed.root != root
            || observed.major_minor != device
            || observed.mount_id != mount_id(&staged)?
        {
            return Err(Reply::Quarantined);
        }
        self.pending = Some(Pending {
            envelope: e.clone(),
            source,
            staged,
            stage: Some(stage),
            destination,
            root,
            device,
        });
        Ok(())
    }

    fn make_private_and_restrict(&mut self, e: &Envelope) -> Result<(), Reply> {
        let p = self.pending.as_ref().ok_or(Reply::Failed)?;
        if p.envelope != *e {
            return Err(Reply::Conflict);
        }
        mounts::make_private(&p.staged)?;
        let view = host_view()?;
        if !at(&view, &path(&p.staged)?)?.optional_fields.is_empty() {
            return Err(Reply::Quarantined);
        }
        self.attributes.workspace(&p.staged)?;
        let view = host_view()?;
        mountinfo::private_stage(at(&view, &path(&p.staged)?)?, &p.root, &p.device)
    }

    fn publish(&mut self, e: &Envelope) -> Result<(), Reply> {
        self.topology()?;
        let p = self.pending.as_mut().ok_or(Reply::Failed)?;
        if p.envelope != *e {
            return Err(Reply::Conflict);
        }
        let holder = e.original.holder.as_ref().ok_or(Reply::Failed)?;
        let current = self.source.enrolled(&holder.enrollment)?;
        same_source(&p.source, &holder.enrollment)?;
        if mount_id(&current)? != mount_id(&p.source)? {
            return Err(Reply::Quarantined);
        }
        scan_source(&p.source)?;
        mounts::attach(&p.staged, p.destination.placeholder()?)?;
        p.stage.take().ok_or(Reply::Failed)?.remove()?;
        self.live.insert(holder.attachment, holder.clone());
        Ok(())
    }

    fn unmount(&mut self, e: &Envelope) -> Result<bool, Reply> {
        let holder = e.original.holder.as_ref().ok_or(Reply::Failed)?;
        if self.revoking.is_some()
            || self.pending.is_some()
            || self.live.get(&holder.attachment) != Some(holder)
        {
            return Err(Reply::Conflict);
        }
        self.topology()?;
        let mount = self.publisher.workspace_mount(&holder.enrollment)?;
        same_source(&mount, &holder.enrollment)?;
        let busy = mounts::ordinary_unmount(&mount)? == Unmount::Busy;
        self.revoking = Some((holder.clone(), mount));
        Ok(busy)
    }

    fn detach(&mut self, e: &Envelope) -> Result<(), Reply> {
        let (holder, mount) = self.revoking.as_ref().ok_or(Reply::Failed)?;
        if e.original.holder.as_ref() != Some(holder) {
            return Err(Reply::Conflict);
        }
        mounts::lazy_detach(mount)
    }

    fn remove_placeholder(&mut self, e: &Envelope) -> Result<(), Reply> {
        let (holder, _) = self.revoking.as_ref().ok_or(Reply::Failed)?;
        if e.original.holder.as_ref() != Some(holder) {
            return Err(Reply::Conflict);
        }
        self.publisher
            .unmounted_destination(&holder.enrollment)?
            .remove()?;
        self.live.remove(&holder.attachment);
        Ok(())
    }

    fn verify(&mut self, e: &Envelope) -> Result<(Vec<MountEvidence>, Vec<MountEvidence>), Reply> {
        let evidence = self.topology()?;
        if e.original.kind == Kind::Revoke {
            let holder = e.original.holder.as_ref().ok_or(Reply::Failed)?;
            let hint = &holder.enrollment;
            absent(
                &self.publisher.fd,
                &format!("{}/{}", hint.project, hint.name),
            )?;
            absent(
                &self.runtime.root,
                &format!("{}/{}/{}", self.receiver_relative, hint.project, hint.name),
            )?;
        }
        self.pending = None;
        self.revoking = None;
        Ok(evidence)
    }
}
