//! Isolated live-workspace helper mechanisms. No host verb or production caller.
//!
//! A peer UID authenticates only the trusted host forwarder, not lease authority.
//! B4 must admit and serialize daemon operations before forwarding them here.
//! No kernel backend is selected by this module and hello never enables B4.

pub mod bootstrap;
mod confinement;
pub mod kernel;
pub mod mountinfo;
pub mod mounts;
mod paths;
pub mod registration_roots;
pub mod runtime;
pub mod service;
mod store;

pub use paths::{Destination, ProtectedRoot, mount_id, scan_source};
pub use store::Store;

use terrarium_core::live_workspace::{
    Delivery, Envelope, Journal, Kind, MAX_FRAME_BYTES, MountEvidence, Phase, Reply, decode_frame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// All methods must act on pinned identities, not request-provided paths/PIDs.
/// A successful transition means its postcondition was independently observed.
/// The implementation must retain namespace, source and destination FDs through
/// final verification. On failure the helper quarantines, never retries a mount.
pub trait Backend {
    fn reconcile(&mut self, journal: &Journal) -> Result<(), Reply>;
    fn check_runtime(&mut self, envelope: &Envelope) -> Result<(), Reply>;
    fn stage(&mut self, envelope: &Envelope) -> Result<(), Reply>;
    fn make_private_and_restrict(&mut self, envelope: &Envelope) -> Result<(), Reply>;
    fn publish(&mut self, envelope: &Envelope) -> Result<(), Reply>;
    /// Return true only for EBUSY with verified, unchanged attachment identity.
    fn unmount(&mut self, envelope: &Envelope) -> Result<bool, Reply>;
    fn detach(&mut self, envelope: &Envelope) -> Result<(), Reply>;
    fn remove_placeholder(&mut self, envelope: &Envelope) -> Result<(), Reply>;
    fn verify(
        &mut self,
        envelope: &Envelope,
    ) -> Result<(Vec<MountEvidence>, Vec<MountEvidence>), Reply>;
}

/// One instance owns an exclusive journal lock and serializes one registration.
pub struct Helper<B> {
    store: Store,
    journal: Journal,
    backend: B,
    fenced: bool,
}

impl<B: Backend> Helper<B> {
    /// Recovered intents never replay kernel actions. Ambiguous state remains
    /// quarantined even if a backend reports that its current mounts look valid.
    pub fn open(store: Store, mut backend: B) -> Result<Self, Reply> {
        let mut journal = store.load()?;
        journal.recover_without_evidence();
        if backend.reconcile(&journal).is_err() {
            journal.quarantined = true;
        }
        tracing::info!(target = %journal.target, registration = ?journal.runtime.registration, quarantined = journal.quarantined, "workspace helper startup reconcile");
        store.save(&journal)?;
        Ok(Self {
            store,
            journal,
            backend,
            fenced: false,
        })
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    fn save(&mut self) -> Result<(), Reply> {
        if self.store.save(&self.journal).is_err() {
            self.fenced = true;
            return Err(Reply::Quarantined);
        }
        Ok(())
    }

    fn advance(&mut self, e: &Envelope, phase: Phase) -> Result<(), Reply> {
        self.journal.advance(e.original.operation, phase)?;
        self.save()
    }

    pub fn apply(&mut self, bytes: &[u8]) -> Result<Reply, Reply> {
        if self.fenced || self.journal.quarantined {
            return Err(Reply::Quarantined);
        }
        let e = decode_frame(bytes)?;
        if matches!(
            e.original.provenance.caller,
            terrarium_core::live_workspace::Caller::Link(_)
        ) && e.original.kind != Kind::Revoke
        {
            return Err(Reply::Unauthorized);
        }
        // Validate supported operations BEFORE writing any intent. Register and
        // lifecycle support require a separately reviewed Docker registrar.
        if !matches!(
            e.original.kind,
            Kind::Grant | Kind::Revoke | Kind::Reconcile
        ) {
            return Err(Reply::Unsupported);
        }
        if let Some(reply) = self.journal.prepare(e.clone())? {
            return Ok(reply);
        }
        self.save()?; // write-ahead intent, including revoke tombstone
        tracing::info!(operation = ?e.original.operation, revision = e.original.revision, kind = ?e.original.kind, host = %e.original.host, target = %e.original.target, runtime = ?e.original.runtime, provenance = ?e.original.provenance, "workspace helper durable admission");
        let result = self.execute(&e);
        if result.is_err() {
            self.journal.quarantined = true;
            self.save()?;
        }
        tracing::info!(operation = ?e.original.operation, revision = e.original.revision, result = ?result, quarantined = self.journal.quarantined, "workspace helper operation result");
        result
    }

    fn execute(&mut self, e: &Envelope) -> Result<Reply, Reply> {
        self.backend.check_runtime(e)?;
        if e.delivery == Delivery::Superseded {
            self.advance(e, Phase::Verified)?;
        } else {
            match e.original.kind {
                Kind::Grant => {
                    self.backend.stage(e)?;
                    self.advance(e, Phase::Staged)?;
                    self.backend.make_private_and_restrict(e)?;
                    self.advance(e, Phase::StageVerified)?;
                    self.backend.publish(e)?;
                    self.advance(e, Phase::Published)?;
                }
                Kind::Revoke => {
                    let busy = self.backend.unmount(e)?;
                    self.advance(e, Phase::OrdinaryUnmountObserved)?;
                    if busy {
                        self.backend.detach(e)?;
                        self.advance(e, Phase::LazyDetachObserved)?;
                    }
                    self.backend.remove_placeholder(e)?;
                    self.advance(e, Phase::PlaceholderRemoved)?;
                }
                Kind::Reconcile => self.backend.reconcile(&self.journal)?,
                _ => return Err(Reply::Unsupported),
            }
            let (host, receiver) = self.backend.verify(e)?;
            let intent = self
                .journal
                .operations
                .get_mut(&e.original.operation)
                .ok_or(Reply::Failed)?;
            intent.host_evidence = host;
            intent.receiver_evidence = receiver;
            self.advance(e, Phase::Verified)?;
        }
        let reply = self.journal.commit(e.original.operation)?;
        self.save()?; // committed result and evidence durable BEFORE reply
        #[cfg(test)]
        crash("committed");
        Ok(reply)
    }
}

/// One bounded request/reply per connection, with receive and send deadlines.
/// Listener creation/ownership is deliberately not part of host-agent startup.
pub async fn serve_connection<B: Backend>(
    helper: &mut Helper<B>,
    stream: UnixStream,
    agent_uid: u32,
) -> std::io::Result<()> {
    serve_frame(stream, agent_uid, |bytes| helper.apply(bytes)).await
}

pub(super) async fn serve_frame(
    mut stream: UnixStream,
    agent_uid: u32,
    mut apply: impl FnMut(&[u8]) -> Result<Reply, Reply>,
) -> std::io::Result<()> {
    use std::time::Duration;
    let peer = stream.peer_cred()?;
    tracing::info!(
        uid = peer.uid(),
        allowed = peer.uid() == agent_uid,
        "workspace helper peer"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        let result = if peer.uid() != agent_uid {
            Err(Reply::Unauthorized)
        } else {
            let mut header = [0; 4];
            stream.read_exact(&mut header).await?;
            let size = u32::from_be_bytes(header) as usize;
            if size == 0 || size > MAX_FRAME_BYTES {
                Err(Reply::Failed)
            } else {
                let mut bytes = vec![0; size];
                stream.read_exact(&mut bytes).await?;
                apply(&bytes)
            }
        };
        let reply = result.unwrap_or_else(|r| r);
        let bytes = serde_json::to_vec(&reply).map_err(std::io::Error::other)?;
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await?;
        stream.write_all(&bytes).await
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "workspace helper frame deadline",
        )
    })?
}

#[cfg(test)]
mod crash_probe;
#[cfg(test)]
mod disposable;

/// Test-only hard process death, never present in the installed helper build.
#[cfg(test)]
fn crash(point: &str) {
    if std::env::var("TERRARIUM_B2_DISPOSABLE").as_deref() == Ok("1")
        && std::env::var("TERRARIUM_B2_CRASH_POINT").as_deref() == Ok(point)
    {
        let _ = nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_CORE, 0, 0);
        // Bypass libtest's thread-local eprintln capture: abort intentionally
        // cannot flush that buffer. The parent requires this exact OS-pipe
        // marker as well as SIGABRT and the expected durable phase.
        let _ = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("B2_CRASH_POINT={point}\n").as_bytes(),
        );
        std::process::abort();
    }
}
#[cfg(test)]
mod tests;
