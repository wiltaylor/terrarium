//! Inactive live-workspace protocol and crash-state model (TER-924).
//!
//! No transport, helper, mount operation, or production caller is provided.
//! Persist an intent with FULL durability before acting, persist verified commit
//! before acknowledging, and only then commit daemon runtime state. B2 supplies
//! the journal fsync/parent-directory fsync and kernel verification mechanisms.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use ulid::Ulid;

pub mod bootstrap;

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

macro_rules! id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Ulid);
    };
}
id!(WorkspaceId);
id!(TrustDomainId);
id!(RegistrationId);
id!(RuntimeId);
id!(AttachmentId);
id!(OperationId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub workspace: WorkspaceId,
    pub host: Ulid,
    pub agent: crate::AgentId,
    pub project: String,
    pub name: String,
    pub source: SourceIdentity,
}

fn segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

impl Enrollment {
    /// Paths and aliases are never selectors. The opened object and immutable
    /// agent/host IDs must match the enrollment as well as the mapping hint.
    pub fn matches(&self, other: &Self) -> bool {
        self == other && segment(&self.project) && segment(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub registration: RegistrationId,
    pub incarnation: RuntimeId,
    /// Full immutable Docker ID, never a name or a PID.
    pub container_id: String,
    pub namespace: NamespaceIdentity,
}

/// Mount IDs are local to `namespace`. Host and receiver records are separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountEvidence {
    pub namespace: NamespaceIdentity,
    pub mount_id: u64,
    pub parent_id: u64,
    pub major_minor: String,
    pub root: String,
    pub mountpoint: String,
    pub vfs_flags: Vec<String>,
    pub super_flags: Vec<String>,
    pub optional_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Register,
    Grant,
    Revoke,
    Start,
    Restart,
    Recreate,
    Destroy,
    Reconcile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Holder {
    pub enrollment: Enrollment,
    pub attachment: AttachmentId,
    pub domain: TrustDomainId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Caller {
    LocalOperator,
    Agent(crate::AgentId),
    Link(Ulid),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub daemon_instance: Ulid,
    pub caller: Caller,
}

/// This original body is immutable, including when authority ends before send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub provenance: Provenance,
    pub operation: OperationId,
    pub host: Ulid,
    pub target: String,
    pub runtime: RuntimeIdentity,
    pub revision: u64,
    pub kind: Kind,
    pub holder: Option<Holder>,
}

impl Request {
    /// Canonical v1 encoding: fixed struct field order, typed fields, no maps.
    pub fn hash(&self) -> Result<[u8; 32], serde_json::Error> {
        Ok(Sha256::digest(serde_json::to_vec(self)?).into())
    }

    fn valid(&self) -> bool {
        self.version == PROTOCOL_VERSION
            && self.revision > 0
            && segment(&self.target)
            && self.runtime.container_id.len() == 64
            && self
                .runtime
                .container_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            && self.runtime.namespace.inode > 0
            && (!matches!(self.kind, Kind::Grant | Kind::Revoke) || self.holder.is_some())
            && self.holder.as_ref().is_none_or(|h| {
                h.enrollment.host == self.host
                    && segment(&h.enrollment.project)
                    && segment(&h.enrollment.name)
                    && h.enrollment.source.inode > 0
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Apply,
    /// A barrier carrying the ORIGINAL request/hash, not a rewritten request.
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub original: Request,
    pub original_hash: [u8; 32],
    pub delivery: Delivery,
}

pub fn decode_frame(bytes: &[u8]) -> Result<Envelope, Reply> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Reply::Failed);
    }
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| Reply::Failed)?;
    if !envelope.original.valid()
        || envelope.original.hash().map_err(|_| Reply::Failed)? != envelope.original_hash
        || (envelope.delivery == Delivery::Superseded && envelope.original.kind != Kind::Grant)
    {
        return Err(Reply::Failed);
    }
    Ok(envelope)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentState {
    AttachedVerified,
    AuthorityEnded,
    RevokePending,
    PathRevokedVerified,
    Quarantined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    Staged,
    StageVerified,
    Published,
    OrdinaryUnmountObserved,
    LazyDetachObserved,
    PlaceholderRemoved,
    Verified,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Unsupported,
    Stale,
    Busy,
    Unauthorized,
    Failed,
    Quarantined,
    Conflict,
    RevisionGap {
        expected: u64,
    },
    Applied,
    Superseded,
    /// Fresh-path disappearance only, NOT descriptor/process revocation.
    PathRevokedVerified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub version: u32,
    pub state_version: u32,
    pub workspace_mount: bool,
    pub ready: bool,
    pub reason: String,
}

impl Capability {
    /// A successful hello alone does not authorize activation.
    pub fn compatible(&self) -> bool {
        self.version == PROTOCOL_VERSION
            && self.state_version == STATE_VERSION
            && self.workspace_mount
            && self.ready
    }
}

/// B1 deliberately cannot deserialize or construct an enabled configuration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    #[default]
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub envelope: Envelope,
    pub expected: ExpectedState,
    pub phase: Phase,
    pub result: Option<Reply>,
    /// Namespace-scoped evidence is recorded independently for each view.
    pub host_evidence: Vec<MountEvidence>,
    pub receiver_evidence: Vec<MountEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedState {
    DestinationAbsent,
    AttachmentPresent,
    RegistrationRecheck,
}

/// Durable daemon record. The intent includes the reservation change in the
/// same FULL-synchronous transaction. A helper acknowledgement never directly
/// implies a daemon runtime commit. B4 supplies the actual transaction/callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonOperation {
    pub envelope: Envelope,
    pub phase: DaemonPhase,
    pub helper_result: Option<Reply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonPhase {
    Intent,
    HelperCommitted,
    RuntimeCommitted,
    Quarantined,
}

impl DaemonOperation {
    pub fn acknowledge(&mut self, receipt: &Envelope, result: Reply) -> Result<(), Reply> {
        let expected = match (self.envelope.delivery, self.envelope.original.kind) {
            (Delivery::Superseded, _) => Reply::Superseded,
            (_, Kind::Revoke) => Reply::PathRevokedVerified,
            _ => Reply::Applied,
        };
        if receipt != &self.envelope || result != expected {
            self.phase = DaemonPhase::Quarantined;
            return Err(Reply::Conflict);
        }
        if self.phase == DaemonPhase::Quarantined {
            return Err(Reply::Quarantined);
        }
        self.helper_result = Some(result);
        if self.phase == DaemonPhase::Intent {
            self.phase = DaemonPhase::HelperCommitted;
        }
        Ok(())
    }

    /// Call only within the same DB transaction that updates runtime.attached.
    pub fn commit_runtime(&mut self) -> Result<(), Reply> {
        if self.phase != DaemonPhase::HelperCommitted || self.helper_result.is_none() {
            return Err(Reply::Failed);
        }
        self.phase = DaemonPhase::RuntimeCommitted;
        Ok(())
    }
}

/// Serializable helper journal schema. Mutations are an in-memory MODEL, not
/// durable I/O. B2 must sync every boundary before the next external action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub state_version: u32,
    pub host: Ulid,
    pub target: String,
    pub runtime: RuntimeIdentity,
    pub last_contiguous_revision: u64,
    pub quarantined: bool,
    pub operations: BTreeMap<OperationId, Intent>,
    pub tombstones: BTreeMap<AttachmentId, u64>,
}

impl Journal {
    /// Exact committed retry returns its old result and must never act again.
    /// New intent must be durably stored before a caller may perform any action.
    pub fn prepare(&mut self, envelope: Envelope) -> Result<Option<Reply>, Reply> {
        let bytes = serde_json::to_vec(&envelope).map_err(|_| Reply::Failed)?;
        decode_frame(&bytes)?;
        let r = &envelope.original;
        if self.state_version != STATE_VERSION {
            return Err(Reply::Unsupported);
        }
        if r.host != self.host || r.target != self.target || r.runtime != self.runtime {
            return Err(Reply::Unauthorized);
        }
        if let Some(old) = self.operations.get(&r.operation) {
            if old.envelope != envelope {
                return Err(Reply::Conflict);
            }
            return old.result.map(Some).ok_or(Reply::Busy);
        }
        if self.quarantined {
            return Err(Reply::Quarantined);
        }
        let expected = self
            .last_contiguous_revision
            .checked_add(1)
            .ok_or(Reply::Failed)?;
        if r.revision < expected {
            return Err(Reply::Stale);
        }
        if r.revision > expected {
            return Err(Reply::RevisionGap { expected });
        }
        if self.operations.values().any(|i| i.result.is_none()) {
            return Err(Reply::Busy);
        }
        if let Some(holder) = &r.holder {
            if r.kind == Kind::Grant && self.tombstones.contains_key(&holder.attachment) {
                return Err(Reply::Stale);
            }
            if r.kind == Kind::Revoke {
                self.tombstones.insert(holder.attachment, r.revision);
            }
        }
        self.operations.insert(
            r.operation,
            Intent {
                expected: match r.kind {
                    Kind::Grant => ExpectedState::DestinationAbsent,
                    Kind::Revoke => ExpectedState::AttachmentPresent,
                    _ => ExpectedState::RegistrationRecheck,
                },
                envelope,
                phase: Phase::Prepared,
                result: None,
                host_evidence: Vec::new(),
                receiver_evidence: Vec::new(),
            },
        );
        Ok(None)
    }

    /// Record ONLY after verifying the corresponding external transition.
    pub fn advance(&mut self, id: OperationId, next: Phase) -> Result<(), Reply> {
        if self.quarantined {
            return Err(Reply::Quarantined);
        }
        let i = self.operations.get_mut(&id).ok_or(Reply::Failed)?;
        let permitted = match (i.envelope.delivery, i.envelope.original.kind) {
            (Delivery::Superseded, _) => i.phase == Phase::Prepared && next == Phase::Verified,
            (_, Kind::Grant) => matches!(
                (i.phase, next),
                (Phase::Prepared, Phase::Staged)
                    | (Phase::Staged, Phase::StageVerified)
                    | (Phase::StageVerified, Phase::Published)
                    | (Phase::Published, Phase::Verified)
            ),
            (_, Kind::Revoke) => matches!(
                (i.phase, next),
                (Phase::Prepared, Phase::OrdinaryUnmountObserved)
                    | (Phase::OrdinaryUnmountObserved, Phase::LazyDetachObserved)
                    | (
                        Phase::OrdinaryUnmountObserved | Phase::LazyDetachObserved,
                        Phase::PlaceholderRemoved
                    )
                    | (Phase::PlaceholderRemoved, Phase::Verified)
            ),
            _ => i.phase == Phase::Prepared && next == Phase::Verified,
        };
        if !permitted {
            return Err(Reply::Failed);
        }
        i.phase = next;
        Ok(())
    }

    /// Sync this commit and its evidence before sending the returned reply.
    pub fn commit(&mut self, id: OperationId) -> Result<Reply, Reply> {
        if self.quarantined {
            return Err(Reply::Quarantined);
        }
        let i = self.operations.get_mut(&id).ok_or(Reply::Failed)?;
        if i.phase != Phase::Verified
            || i.result.is_some()
            || self.last_contiguous_revision.checked_add(1) != Some(i.envelope.original.revision)
        {
            return Err(Reply::Failed);
        }
        let reply = match (i.envelope.delivery, i.envelope.original.kind) {
            (Delivery::Superseded, _) => Reply::Superseded,
            (_, Kind::Revoke) => Reply::PathRevokedVerified,
            _ => Reply::Applied,
        };
        i.phase = Phase::Committed;
        i.result = Some(reply);
        self.last_contiguous_revision = i.envelope.original.revision;
        Ok(reply)
    }

    /// Without fresh kernel evidence EVERY uncommitted crash state is ambiguous.
    /// Never infer success from a saved phase or retry a kernel mutation blindly.
    pub fn recover_without_evidence(&mut self) {
        let mut revisions = Vec::new();
        let mut expected_tombstones = BTreeMap::new();
        let mut divergent = false;
        for (id, i) in &self.operations {
            let r = &i.envelope.original;
            let expected_result = match (i.envelope.delivery, r.kind) {
                (Delivery::Superseded, _) => Reply::Superseded,
                (_, Kind::Revoke) => Reply::PathRevokedVerified,
                _ => Reply::Applied,
            };
            divergent |= *id != r.operation
                || r.host != self.host
                || r.target != self.target
                || r.runtime != self.runtime
                || !r.valid()
                || r.hash().ok() != Some(i.envelope.original_hash)
                || i.phase != Phase::Committed
                || i.result != Some(expected_result)
                || (i.envelope.delivery == Delivery::Superseded && r.kind != Kind::Grant);
            revisions.push(r.revision);
            if r.kind == Kind::Revoke
                && let Some(holder) = &r.holder
            {
                // prepare records the latest revoke intent for an attachment.
                // Pending intents still quarantine below, but must not erase
                // their write-ahead tombstone during recovery.
                let revision = expected_tombstones.entry(holder.attachment).or_insert(0);
                *revision = (*revision).max(r.revision);
            }
        }
        divergent |= self.tombstones != expected_tombstones;
        revisions.sort_unstable();
        divergent |= revisions.len() as u64 != self.last_contiguous_revision
            || revisions
                .iter()
                .enumerate()
                .any(|(n, &r)| r != n as u64 + 1);
        if self.state_version != STATE_VERSION
            || divergent
            || self.operations.values().any(|i| i.result.is_none())
        {
            self.quarantined = true;
        }
    }
}
