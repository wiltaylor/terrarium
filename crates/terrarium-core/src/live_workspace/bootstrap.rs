//! Registration bootstrap ordering only. Callers must durably sync each state
//! before its next external action. This model neither provisions nor verifies
//! a filesystem, Docker runtime or namespace, and cannot activate the feature.

use super::{
    Caller, OperationId, PROTOCOL_VERSION, Provenance, RegistrationId, Reply, RuntimeId,
    RuntimeIdentity, SourceIdentity, TrustDomainId, segment,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ulid::Ulid;

/// No administrative paths or caller-selected Docker options are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub operation: OperationId,
    pub provenance: Provenance,
    pub host: Ulid,
    pub target: String,
    pub registration: RegistrationId,
    pub incarnation: RuntimeId,
    pub domain: TrustDomainId,
}

impl Request {
    fn valid(&self) -> bool {
        self.version == PROTOCOL_VERSION
            && segment(&self.target)
            && !matches!(self.provenance.caller, Caller::Link(_))
    }

    pub fn hash(&self) -> Result<[u8; 32], serde_json::Error> {
        Ok(Sha256::digest(serde_json::to_vec(self)?).into())
    }
}

/// Pinned identities observed by the helper beneath its protected parents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scaffold {
    pub publisher: SourceIdentity,
    pub staging: SourceIdentity,
    pub state: SourceIdentity,
}

impl Scaffold {
    fn valid(&self) -> bool {
        self.publisher.inode > 0
            && self.staging.inode > 0
            && self.state.inode > 0
            && self.publisher != self.staging
            && self.publisher != self.state
            && self.staging != self.state
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Intent,
    ScaffoldVerified(Scaffold),
    CreatePending,
    ContainerBound(String),
    ReceiverVerified(RuntimeIdentity),
    Committed,
}

/// Bootstrap history is append-only. The original request is never rewritten
/// to include a container ID discovered after create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    original: Request,
    original_hash: [u8; 32],
    steps: Vec<Step>,
    quarantined: bool,
}

fn container_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Journal {
    pub fn new(original: Request) -> Result<Self, Reply> {
        if !original.valid() {
            return Err(Reply::Unauthorized);
        }
        let original_hash = original.hash().map_err(|_| Reply::Failed)?;
        Ok(Self {
            original,
            original_hash,
            steps: vec![Step::Intent],
            quarantined: false,
        })
    }

    pub fn original(&self) -> &Request {
        &self.original
    }

    /// Only the intent may be installed as the first durable snapshot.
    pub fn initial(&self) -> bool {
        self.valid() && !self.quarantined && self.steps == [Step::Intent]
    }

    /// A snapshot writer may persist one observed transition or quarantine,
    /// never truncate history, skip durability boundaries or clear a fence.
    pub fn follows(&self, old: &Self) -> bool {
        self.valid()
            && old.valid()
            && self.original == old.original
            && self.original_hash == old.original_hash
            && (!old.quarantined || self.quarantined)
            && self.steps.starts_with(&old.steps)
            && (self.steps.len() == old.steps.len()
                || (!old.quarantined && self.steps.len() == old.steps.len() + 1))
    }

    pub fn quarantined(&self) -> bool {
        self.quarantined
    }

    pub fn committed(&self) -> bool {
        !self.quarantined && self.steps.last() == Some(&Step::Committed) && self.valid()
    }

    fn valid(&self) -> bool {
        if !self.original.valid()
            || self.original.hash().ok() != Some(self.original_hash)
            || self.steps.first() != Some(&Step::Intent)
            || self.steps.len() > 6
        {
            return false;
        }
        self.steps
            .windows(2)
            .all(|pair| self.edge(&pair[0], &pair[1]))
    }

    fn edge(&self, from: &Step, to: &Step) -> bool {
        match (from, to) {
            (Step::Intent, Step::ScaffoldVerified(s)) => s.valid(),
            (Step::ScaffoldVerified(_), Step::CreatePending)
            | (Step::ReceiverVerified(_), Step::Committed) => true,
            (Step::CreatePending, Step::ContainerBound(id)) => container_id(id),
            (Step::ContainerBound(id), Step::ReceiverVerified(runtime)) => {
                runtime.registration == self.original.registration
                    && runtime.incarnation == self.original.incarnation
                    && runtime.container_id == *id
                    && runtime.namespace.inode > 0
            }
            _ => false,
        }
    }

    /// The caller must independently verify the corresponding external
    /// postcondition before recording it, then sync before any next action.
    pub fn advance(&mut self, step: Step) -> Result<(), Reply> {
        if self.quarantined || !self.valid() {
            self.quarantined = true;
            return Err(Reply::Quarantined);
        }
        if !self.steps.last().is_some_and(|from| self.edge(from, &step)) {
            return Err(Reply::Failed);
        }
        self.steps.push(step);
        Ok(())
    }

    /// Exact committed retries return a stored result, never a new create.
    /// In-flight retries require explicit reconciliation, not another action.
    pub fn retry(&self, request: &Request) -> Result<(), Reply> {
        if request != &self.original || request.hash().ok() != Some(self.original_hash) {
            return Err(Reply::Failed);
        }
        if !self.committed() {
            return Err(Reply::Quarantined);
        }
        Ok(())
    }

    /// No automatic adoption from names, labels or an empty directory.
    /// A later evidence-based reconciler must prove durable ownership too.
    pub fn recover_without_evidence(&mut self) {
        if !self.committed() {
            self.quarantined = true;
        }
    }
}
