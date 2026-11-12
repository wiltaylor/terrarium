//! Durable grant/revoke ordering and receipt ingestion. There is no transport
//! or production caller here. Dispatch must serialize across helper I/O, not
//! merely across these transactions. No function enables a target.
use super::*;
use terrarium_core::live_workspace::{
    AttachmentId, Caller, DaemonOperation, DaemonPhase, Delivery, Envelope, Holder, Kind,
    NamespaceIdentity, OperationId, PROTOCOL_VERSION, Provenance, RegistrationId, Reply, Request,
    RuntimeId, RuntimeIdentity, decode_frame,
};

/// Internal daemon input, not a wire payload. The sandbox incarnation must be
/// selected from the owning sandbox by the daemon, never a request pathname.
pub struct Grant {
    pub provenance: Provenance,
    pub operation: OperationId,
    pub registration: RegistrationId,
    pub holder: Holder,
    pub owning_sandbox: String,
}

/// Allocate an immutable grant intent and its reservation in one FULL immediate
/// transaction. Errors must abort the caller's transaction. This allocates no
/// attachment and performs no helper I/O. Activation remains disabled.
#[allow(clippy::type_complexity)]
pub fn allocate_grant(t: &Tx<'_>, grant: &Grant) -> ApiResult<Envelope> {
    durable(t)?;
    no_link(t)?;
    if matches!(grant.provenance.caller, Caller::Link(_)) {
        return Err(ApiError::new(
            Code::LinkForbidden,
            "link provenance cannot allocate a grant",
        ));
    }
    if matches!(grant.provenance.caller, Caller::Agent(agent) if agent != grant.holder.enrollment.agent)
    {
        return Err(ApiError::new(
            Code::Forbidden,
            "grant caller does not own the holder",
        ));
    }
    let operation = grant.operation.0.to_string();
    let registration = grant.registration.0.to_string();
    let exists: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM live_workspace_operations WHERE operation_id = ?1)",
        [&operation],
        |r| r.get(0),
    )?;
    if exists {
        let saved = load(t, &operation)?;
        let original = &saved.envelope.original;
        let sandbox: Option<String> = t.query_row(
            "SELECT owning_sandbox_incarnation FROM workspace_reservations WHERE workspace_id = ?1",
            [grant.holder.enrollment.workspace.0.to_string()],
            |r| r.get(0),
        )?;
        if original.kind != Kind::Grant
            || original.provenance != grant.provenance
            || original.runtime.registration != grant.registration
            || original.holder.as_ref() != Some(&grant.holder)
            || sandbox.as_deref() != Some(grant.owning_sandbox.as_str())
        {
            return Err(conflict());
        }
        return Ok(saved.envelope);
    }
    let (host, target, domain, runtime, container, dev, ino, allocated, state): (
        String, String, String, String, String, String, String, u64, String,
    ) = t.query_row(
        "SELECT host_id,target_id,trust_domain_id,runtime_id,container_id,namespace_dev,
         namespace_ino,allocated_revision,state FROM live_workspace_targets WHERE registration_id = ?1",
        [&registration], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
    )?;
    let revision = allocated
        .checked_add(1)
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(conflict)?;
    if state != "verified"
        || host != grant.holder.enrollment.host.to_string()
        || domain != grant.holder.domain.0.to_string()
    {
        return Err(conflict());
    }
    // Attachment IDs are permanent. A tombstone or any previous grant with
    // this attachment ID cannot be reinterpreted as a new publication.
    let used: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM live_workspace_attachments WHERE attachment_id = ?1)",
        [grant.holder.attachment.0.to_string()],
        |r| r.get(0),
    )?;
    if used {
        return Err(conflict());
    }
    let mut pending =
        t.prepare("SELECT original_request FROM live_workspace_operations WHERE kind = 'grant'")?;
    for bytes in pending.query_map([], |r| r.get::<_, Vec<u8>>(0))? {
        let previous: Request = serde_json::from_slice(&bytes?).map_err(|_| conflict())?;
        if previous
            .holder
            .is_none_or(|h| h.attachment == grant.holder.attachment)
        {
            return Err(conflict());
        }
    }
    let request = Request {
        version: PROTOCOL_VERSION,
        provenance: grant.provenance.clone(),
        operation: grant.operation,
        host: host.parse().map_err(|_| conflict())?,
        target,
        runtime: RuntimeIdentity {
            registration: grant.registration,
            incarnation: RuntimeId(runtime.parse().map_err(|_| conflict())?),
            container_id: container,
            namespace: NamespaceIdentity {
                device: dev.parse().map_err(|_| conflict())?,
                inode: ino.parse().map_err(|_| conflict())?,
            },
        },
        revision,
        kind: Kind::Grant,
        holder: Some(grant.holder.clone()),
    };
    let envelope = Envelope {
        original_hash: request.hash().map_err(|_| conflict())?,
        original: request,
        delivery: Delivery::Apply,
    };
    decode_frame(&serde_json::to_vec(&envelope).map_err(|_| conflict())?)
        .map_err(|_| conflict())?;
    super::reserve(
        t,
        &grant.holder.enrollment,
        &registration,
        &domain,
        &grant.owning_sandbox,
    )?;
    t.execute("INSERT INTO live_workspace_operations VALUES (?1,?2,?3,'grant',?4,?5,'apply','intent',NULL)",
        params![operation, registration, revision, serde_json::to_vec(&envelope.original).map_err(|_| conflict())?, envelope.original_hash.as_slice()])?;
    let changed = t.execute("UPDATE live_workspace_targets SET allocated_revision = ?2 WHERE registration_id = ?1 AND allocated_revision = ?3",
        params![registration, revision, allocated])?;
    if changed != 1 {
        return Err(conflict());
    }
    t.audit("workspace.grant_intent", &operation, Some(&grant.holder.enrollment.project),
        json!({"registration": registration, "revision": revision, "attachment": grant.holder.attachment.0.to_string()}))?;
    Ok(envelope)
}

/// The caller must hold the same per-registration serialization as dispatch.
/// Ownership comes from the original durable grant, not a replacement holder
/// supplied by a revoke request. This function never unseals a workspace.
#[allow(clippy::type_complexity)]
pub fn allocate_revoke(
    t: &Tx<'_>,
    provenance: &Provenance,
    operation: OperationId,
    registration: RegistrationId,
    attachment: AttachmentId,
) -> ApiResult<Envelope> {
    durable(t)?;
    no_link(t)?;
    if matches!(provenance.caller, Caller::Link(_)) {
        return Err(ApiError::new(
            Code::LinkForbidden,
            "link provenance cannot revoke a workspace",
        ));
    }
    let registration_id = registration.0.to_string();
    let operation_id = operation.0.to_string();
    let exists: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM live_workspace_operations WHERE operation_id = ?1)",
        [&operation_id],
        |r| r.get(0),
    )?;
    if exists {
        let saved = load(t, &operation_id)?;
        if saved.envelope.original.kind != Kind::Revoke
            || saved.envelope.original.provenance != *provenance
            || saved.envelope.original.runtime.registration != registration
            || saved
                .envelope
                .original
                .holder
                .as_ref()
                .is_none_or(|h| h.attachment != attachment)
        {
            return Err(conflict());
        }
        return Ok(saved.envelope);
    }
    let mut grants = t.prepare("SELECT operation_id FROM live_workspace_operations WHERE registration_id = ?1 AND kind = 'grant'")?;
    let mut owner = None;
    for id in grants.query_map([&registration_id], |r| r.get::<_, String>(0))? {
        let candidate = load(t, &id?)?;
        if candidate
            .envelope
            .original
            .holder
            .as_ref()
            .is_some_and(|h| h.attachment == attachment)
        {
            if owner.is_some() {
                return Err(conflict());
            }
            owner = Some(candidate);
        }
    }
    let grant = owner.ok_or_else(conflict)?;
    let holder = grant
        .envelope
        .original
        .holder
        .as_ref()
        .ok_or_else(conflict)?;
    if matches!(provenance.caller, Caller::Agent(agent) if agent != holder.enrollment.agent) {
        return Err(ApiError::new(
            Code::Forbidden,
            "revoke caller does not own the holder",
        ));
    }
    super::identity(t, &holder.enrollment)?;
    let valid: bool = t.query_row("SELECT EXISTS(SELECT 1 FROM workspace_reservations WHERE workspace_id = ?1 AND trust_domain_id = ?2 AND current_registration_id = ?3)",
        params![holder.enrollment.workspace.0.to_string(), holder.domain.0.to_string(), registration_id], |r| r.get(0))?;
    if !valid {
        return Err(conflict());
    }
    let allocated: u64 = t.query_row("SELECT allocated_revision FROM live_workspace_targets WHERE registration_id = ?1 AND state != 'destroyed'", [&registration_id], |r| r.get(0))?;
    let revision = allocated
        .checked_add(1)
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(conflict)?;
    let existing: Option<(String, String, String, Option<u64>)> = t.query_row(
        "SELECT workspace_id, registration_id, agent_id, revoke_revision FROM live_workspace_attachments WHERE attachment_id = ?1",
        [attachment.0.to_string()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).optional()?;
    if let Some((workspace, registered, agent, tombstone)) = &existing
        && (workspace != &holder.enrollment.workspace.0.to_string()
            || registered != &registration_id
            || agent != &holder.enrollment.agent.to_string()
            || tombstone.is_some())
    {
        return Err(conflict());
    }
    let mut request = grant.envelope.original.clone();
    request.operation = operation;
    request.provenance = provenance.clone();
    request.kind = Kind::Revoke;
    request.revision = revision;
    let envelope = Envelope {
        original_hash: request.hash().map_err(|_| conflict())?,
        original: request,
        delivery: Delivery::Apply,
    };
    if existing.is_some() {
        t.execute("UPDATE live_workspace_attachments SET state = 'authority_ended' WHERE attachment_id = ?1 AND state = 'attached_verified'", [attachment.0.to_string()])?;
        t.execute("UPDATE live_workspace_attachments SET state = 'revoke_pending', revoke_revision = ?2 WHERE attachment_id = ?1",
            params![attachment.0.to_string(), revision])?;
    } else {
        // A grant not yet runtime-committed still needs a durable tombstone.
        t.execute(
            "INSERT INTO live_workspace_attachments VALUES (?1,?2,?3,?4,'revoke_pending',?5)",
            params![
                attachment.0.to_string(),
                holder.enrollment.workspace.0.to_string(),
                registration_id,
                holder.enrollment.agent.to_string(),
                revision
            ],
        )?;
    }
    t.execute("UPDATE live_workspace_operations SET delivery = 'superseded' WHERE operation_id = ?1 AND phase = 'intent' AND delivery = 'apply'",
        [grant.envelope.original.operation.0.to_string()])?;
    t.execute("INSERT INTO live_workspace_operations VALUES (?1,?2,?3,'revoke',?4,?5,'apply','intent',NULL)",
        params![operation_id, registration_id, revision, serde_json::to_vec(&envelope.original).map_err(|_| conflict())?, envelope.original_hash.as_slice()])?;
    t.execute(
        "UPDATE live_workspace_targets SET allocated_revision = ?2 WHERE registration_id = ?1",
        params![registration_id, revision],
    )?;
    t.audit("workspace.revoke_intent", &operation_id, Some(&holder.enrollment.project),
        json!({"registration": registration_id, "revision": revision, "attachment": attachment.0.to_string()}))?;
    Ok(envelope)
}

fn conflict() -> ApiError {
    ApiError::new(Code::Conflict, "invalid live workspace operation record")
}

fn revoked(t: &Tx<'_>, holder: &Holder) -> ApiResult<bool> {
    Ok(t.query_row("SELECT EXISTS(SELECT 1 FROM live_workspace_attachments WHERE attachment_id = ?1 AND revoke_revision IS NOT NULL)",
        [holder.attachment.0.to_string()], |r| r.get(0))?)
}

fn still_authorized(t: &Tx<'_>, holder: &Holder, registration: &str) -> ApiResult<bool> {
    super::identity(t, &holder.enrollment)?;
    let reserved: bool = t.query_row("SELECT EXISTS(SELECT 1 FROM workspace_reservations WHERE workspace_id = ?1 AND trust_domain_id = ?2 AND current_registration_id = ?3)",
        params![holder.enrollment.workspace.0.to_string(), holder.domain.0.to_string(), registration], |r| r.get(0))?;
    Ok(reserved
        && !revoked(t, holder)?
        && super::current_authority(t, &holder.enrollment, registration)?)
}

fn cancel_grant(t: &Tx<'_>, stored: &DaemonOperation) -> ApiResult<()> {
    let holder = stored
        .envelope
        .original
        .holder
        .as_ref()
        .ok_or_else(conflict)?;
    if !revoked(t, holder)? {
        allocate_revoke(
            t,
            &Provenance {
                daemon_instance: stored.envelope.original.provenance.daemon_instance,
                caller: Caller::LocalOperator,
            },
            OperationId(Ulid::new()),
            stored.envelope.original.runtime.registration,
            holder.attachment,
        )?;
    }
    // Never rewrite a helper-committed request or an original body/hash.
    t.execute("UPDATE live_workspace_operations SET delivery = 'superseded' WHERE operation_id = ?1 AND phase = 'intent' AND delivery = 'apply'",
        [stored.envelope.original.operation.0.to_string()])?;
    Ok(())
}

/// Select only the next contiguous revision. The caller must hold a single
/// per-registration lock across this transaction, helper I/O, receipt ingestion
/// and runtime commit. This function alone is not a dispatch claim or lease.
pub fn next(t: &Tx<'_>, registration: RegistrationId) -> ApiResult<Option<DaemonOperation>> {
    durable(t)?;
    no_link(t)?;
    let id = registration.0.to_string();
    let (last, allocated, state): (u64, u64, String) = t.query_row(
        "SELECT last_contiguous_revision,allocated_revision,state FROM live_workspace_targets WHERE registration_id = ?1",
        [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if state != "verified" {
        return Err(conflict());
    }
    if last == allocated {
        return Ok(None);
    }
    let revision = last.checked_add(1).ok_or_else(conflict)?;
    let operation: String = t.query_row("SELECT operation_id FROM live_workspace_operations WHERE registration_id = ?1 AND revision = ?2",
        params![id, revision], |r| r.get(0))?;
    let stored = load(t, &operation)?;
    if !matches!(
        stored.phase,
        DaemonPhase::Intent | DaemonPhase::HelperCommitted
    ) {
        return Err(conflict());
    }
    if stored.envelope.original.kind == Kind::Grant {
        let holder = stored
            .envelope
            .original
            .holder
            .as_ref()
            .ok_or_else(conflict)?;
        if !still_authorized(t, holder, &id)? {
            cancel_grant(t, &stored)?;
        }
    }
    Ok(Some(load(t, &operation)?))
}

/// Commit only a saved exact helper receipt, in the same transaction as the
/// attachment mutation and contiguous barrier. Renew authority rather than
/// treating the earlier helper result as a current authorization decision.
pub fn commit_runtime(t: &Tx<'_>, operation: OperationId) -> ApiResult<()> {
    durable(t)?;
    no_link(t)?;
    let id = operation.0.to_string();
    let stored = load(t, &id)?;
    let request = &stored.envelope.original;
    let registration = request.runtime.registration.0.to_string();
    let (last, state): (u64, String) = t.query_row("SELECT last_contiguous_revision,state FROM live_workspace_targets WHERE registration_id = ?1",
        [&registration], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if stored.phase == DaemonPhase::RuntimeCommitted && request.revision <= last {
        return Ok(());
    }
    if state != "verified"
        || last.checked_add(1) != Some(request.revision)
        || stored.phase != DaemonPhase::HelperCommitted
    {
        return Err(conflict());
    }
    let mut verified = stored.clone();
    verified
        .acknowledge(&stored.envelope, stored.helper_result.ok_or_else(conflict)?)
        .map_err(|_| conflict())?;
    match (request.kind, stored.envelope.delivery) {
        (Kind::Grant, Delivery::Apply) => {
            let holder = request.holder.as_ref().ok_or_else(conflict)?;
            if still_authorized(t, holder, &registration)? {
                t.execute("INSERT INTO live_workspace_attachments VALUES (?1,?2,?3,?4,'attached_verified',NULL)",
                    params![holder.attachment.0.to_string(), holder.enrollment.workspace.0.to_string(), registration, holder.enrollment.agent.to_string()])?;
            } else {
                // The helper may already have published. Retain the seal and
                // ordered revoke without admitting exec or claiming absence.
                cancel_grant(t, &stored)?;
            }
        }
        (Kind::Grant, Delivery::Superseded) => {
            let holder = request.holder.as_ref().ok_or_else(conflict)?;
            if !revoked(t, holder)? {
                return Err(conflict());
            }
        }
        (Kind::Revoke, Delivery::Apply) => {
            let holder = request.holder.as_ref().ok_or_else(conflict)?;
            let updated = t.execute("UPDATE live_workspace_attachments SET state = 'path_revoked_verified' WHERE attachment_id = ?1 AND registration_id = ?2 AND revoke_revision = ?3 AND state = 'revoke_pending'",
                params![holder.attachment.0.to_string(), registration, request.revision])?;
            if updated != 1 {
                return Err(conflict());
            }
        }
        _ => return Err(conflict()), // Lifecycle runtime commits need their own verified integration.
    }
    verified.commit_runtime().map_err(|_| conflict())?;
    t.execute(
        "UPDATE live_workspace_operations SET phase = 'daemon_committed' WHERE operation_id = ?1",
        [&id],
    )?;
    t.execute("UPDATE live_workspace_targets SET last_contiguous_revision = ?2 WHERE registration_id = ?1",
        params![registration, request.revision])?;
    t.audit("workspace.runtime_committed", &id, request.holder.as_ref().map(|h| h.enrollment.project.as_str()),
        json!({"registration": registration, "revision": request.revision, "kind": request.kind, "delivery": stored.envelope.delivery}))?;
    Ok(())
}

#[allow(clippy::type_complexity)]
fn load(t: &Tx<'_>, operation: &str) -> ApiResult<DaemonOperation> {
    let (registration, revision, kind, bytes, hash, delivery, phase, result): (
        String,
        u64,
        String,
        Vec<u8>,
        Vec<u8>,
        String,
        String,
        Option<Vec<u8>>,
    ) = t.query_row(
        "SELECT registration_id, revision, kind, original_request, original_hash,
         delivery, phase, result FROM live_workspace_operations WHERE operation_id = ?1",
        [operation],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        },
    )?;
    let original: Request = serde_json::from_slice(&bytes).map_err(|_| conflict())?;
    let envelope = Envelope {
        original,
        original_hash: hash.try_into().map_err(|_| conflict())?,
        delivery: match delivery.as_str() {
            "apply" => Delivery::Apply,
            "superseded" => Delivery::Superseded,
            _ => return Err(conflict()),
        },
    };
    let frame = serde_json::to_vec(&envelope).map_err(|_| conflict())?;
    decode_frame(&frame).map_err(|_| conflict())?;
    if envelope.original.operation.0.to_string() != operation
        || envelope.original.runtime.registration.0.to_string() != registration
        || envelope.original.revision != revision
        || serde_json::to_value(envelope.original.kind).map_err(|_| conflict())? != kind
        || matches!(envelope.original.provenance.caller, Caller::Link(_))
        || serde_json::to_vec(&envelope.original).map_err(|_| conflict())? != bytes
    {
        return Err(conflict());
    }
    let phase = match phase.as_str() {
        "intent" => DaemonPhase::Intent,
        "helper_committed" => DaemonPhase::HelperCommitted,
        "daemon_committed" => DaemonPhase::RuntimeCommitted,
        "quarantined" => DaemonPhase::Quarantined,
        _ => return Err(conflict()),
    };
    let helper_result = result
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| conflict()))
        .transpose()?;
    if matches!(
        phase,
        DaemonPhase::HelperCommitted | DaemonPhase::RuntimeCommitted
    ) && helper_result.is_none()
    {
        return Err(conflict());
    }
    Ok(DaemonOperation {
        envelope,
        phase,
        helper_result,
    })
}

/// Return quarantine as a successful transaction value, not an error which
/// would roll back the fence. The caller must commit this FULL transaction.
/// Transport authentication and per-registration I/O serialization are still
/// required before calling this function. Receipt equality is not either one.
#[allow(clippy::type_complexity)]
pub fn acknowledge(t: &Tx<'_>, receipt: &Envelope, result: Reply) -> ApiResult<Reply> {
    durable(t)?;
    no_link(t)?;
    let operation = receipt.original.operation.0.to_string();
    // Select the quarantine target from durable ownership, never the receipt.
    let registration: String = t.query_row(
        "SELECT registration_id FROM live_workspace_operations WHERE operation_id = ?1",
        [&operation],
        |r| r.get(0),
    )?;
    let valid = (|| -> ApiResult<DaemonOperation> {
        let mut stored = load(t, &operation)?;
        let r = &stored.envelope.original;
        let (host, target, runtime, container, dev, ino, state, last): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            u64,
        ) = t.query_row(
            "SELECT host_id, target_id, runtime_id, container_id, namespace_dev,
             namespace_ino, state, last_contiguous_revision FROM live_workspace_targets
             WHERE registration_id = ?1",
            [&registration],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        let next = last.checked_add(1).ok_or_else(conflict)?;
        if host != r.host.to_string()
            || target != r.target
            || runtime != r.runtime.incarnation.0.to_string()
            || container != r.runtime.container_id
            || dev != r.runtime.namespace.device.to_string()
            || ino != r.runtime.namespace.inode.to_string()
            || state != "verified"
            || match stored.phase {
                DaemonPhase::RuntimeCommitted => r.revision > last,
                _ => r.revision != next,
            }
            || stored.helper_result.is_some_and(|saved| saved != result)
        {
            return Err(conflict());
        }
        stored
            .acknowledge(receipt, result)
            .map_err(|_| conflict())?;
        Ok(stored)
    })();
    if valid.is_err() {
        t.execute(
            "UPDATE live_workspace_operations SET phase = 'quarantined' WHERE operation_id = ?1",
            [&operation],
        )?;
        t.execute("UPDATE live_workspace_targets SET state = 'quarantined' WHERE registration_id = ?1 AND state != 'destroyed'", [&registration])?;
        t.audit("workspace.receipt_quarantined", &operation, None,
            json!({"registration": registration, "reason": "receipt_or_order_mismatch"}))?;
        return Ok(Reply::Quarantined);
    }
    let stored = valid?;
    if stored.phase == DaemonPhase::HelperCommitted {
        let bytes = serde_json::to_vec(&result).map_err(|_| conflict())?;
        let changed = t.execute("UPDATE live_workspace_operations SET phase = 'helper_committed', result = ?2 WHERE operation_id = ?1 AND phase = 'intent'",
            params![operation, bytes])?;
        if changed == 1 {
            t.audit("workspace.helper_committed", &operation, stored.envelope.original.holder.as_ref().map(|h| h.enrollment.project.as_str()),
                json!({"registration": registration, "revision": stored.envelope.original.revision, "result": result}))?;
        }
    }
    // No revision, attachment, reservation or runtime mutation here.
    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{db::Db, events::EventHub};
    use terrarium_core::live_workspace::{
        Kind, NamespaceIdentity, OperationId, PROTOCOL_VERSION, Provenance, RegistrationId,
        RuntimeId, RuntimeIdentity,
    };

    fn insert(t: &mut Tx<'_>) -> ApiResult<Envelope> {
        let r = Request {
            version: PROTOCOL_VERSION,
            provenance: Provenance {
                daemon_instance: Ulid::new(),
                caller: Caller::LocalOperator,
            },
            operation: OperationId(Ulid::new()),
            host: Ulid::new(),
            target: "synthetic".into(),
            runtime: RuntimeIdentity {
                registration: RegistrationId(Ulid::new()),
                incarnation: RuntimeId(Ulid::new()),
                container_id: "a".repeat(64),
                namespace: NamespaceIdentity {
                    device: 1,
                    inode: 2,
                },
            },
            revision: 1,
            kind: Kind::Reconcile,
            holder: None,
        };
        let registration = r.runtime.registration.0.to_string();
        t.execute("INSERT INTO live_workspace_targets (registration_id,host_id,target_id,trust_domain_id,runtime_id,container_id,namespace_dev,namespace_ino)
            VALUES (?1,?2,?3,?4,?5,?6,'1','2')",
            params![registration, r.host.to_string(), r.target, Ulid::new().to_string(), r.runtime.incarnation.0.to_string(), r.runtime.container_id])?;
        t.execute("UPDATE live_workspace_targets SET state = 'reconcile_pending' WHERE registration_id = ?1", [&registration])?;
        t.execute("UPDATE live_workspace_targets SET state = 'verified', allocated_revision = 1 WHERE registration_id = ?1", [&registration])?;
        let envelope = Envelope {
            original_hash: r.hash().unwrap(),
            original: r,
            delivery: Delivery::Apply,
        };
        t.execute("INSERT INTO live_workspace_operations VALUES (?1,?2,1,'reconcile',?3,?4,'apply','intent',NULL)",
            params![envelope.original.operation.0.to_string(), registration, serde_json::to_vec(&envelope.original).unwrap(), envelope.original_hash.as_slice()])?;
        Ok(envelope)
    }

    #[tokio::test]
    async fn exact_receipt_is_durable_but_never_commits_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(
            crate::db::open(&dir.path().join("test.sqlite")).unwrap(),
            EventHub::default(),
        );
        let receipt = db.write_full("system", insert).await.unwrap();
        let check = receipt.clone();
        db.write_full("system", move |t| {
            assert_eq!(acknowledge(t, &check, Reply::Applied)?, Reply::Applied);
            assert_eq!(acknowledge(t, &check, Reply::Applied)?, Reply::Applied);
            let record = load(t, &check.original.operation.0.to_string())?;
            assert_eq!(record.phase, DaemonPhase::HelperCommitted);
            let last: u64 = t.query_row(
                "SELECT last_contiguous_revision FROM live_workspace_targets",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(last, 0);
            Ok(())
        })
        .await
        .unwrap();
        db.write_full("system", move |t| {
            let saved = load(t, &receipt.original.operation.0.to_string())?;
            assert_eq!(saved.helper_result, Some(Reply::Applied));
            assert_eq!(saved.phase, DaemonPhase::HelperCommitted);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn receipt_substitution_fences_durable_owner_not_supplied_registration() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(
            crate::db::open(&dir.path().join("test.sqlite")).unwrap(),
            EventHub::default(),
        );
        let mut receipt = db.write_full("system", insert).await.unwrap();
        let operation = receipt.original.operation.0.to_string();
        receipt.original.runtime.registration = RegistrationId(Ulid::new());
        db.write_full("system", move |t| {
            assert_eq!(
                acknowledge(t, &receipt, Reply::Applied)?,
                Reply::Quarantined
            );
            Ok(()) // Commit the fence rather than returning an error.
        })
        .await
        .unwrap();
        db.write_full("system", move |t| {
            assert_eq!(load(t, &operation)?.phase, DaemonPhase::Quarantined);
            let state: String =
                t.query_row("SELECT state FROM live_workspace_targets", [], |r| r.get(0))?;
            assert_eq!(state, "quarantined");
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn grant_allocation_seals_once_and_retains_original_on_expired_retry() {
        use terrarium_core::live_workspace::{
            AttachmentId, SourceIdentity, TrustDomainId, WorkspaceId,
        };
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(
            crate::db::open(&dir.path().join("test.sqlite")).unwrap(),
            EventHub::default(),
        );
        let grant = db.write_full("system", |t| {
            let seed = insert(t)?;
            let enrollment = Enrollment {
                workspace: WorkspaceId(Ulid::new()), host: seed.original.host,
                agent: terrarium_core::AgentId::new(), project: "fixture".into(), name: "holder".into(),
                source: SourceIdentity { device: 1, inode: 2 },
            };
            let domain: String = t.query_row("SELECT trust_domain_id FROM live_workspace_targets", [], |r| r.get(0))?;
            t.execute("INSERT INTO agents (id,name,project,harness,workspace,created_at,updated_at,status,policy,source)
                VALUES (?1,'holder','fixture','codex','/synthetic','now','now','idle',?2,'api')",
                params![enrollment.agent.to_string(), r#"{"targets":["synthetic"]}"#])?;
            t.execute("INSERT INTO live_workspaces VALUES (?1,?2,?3,'fixture','holder','1','2','fresh')",
                params![enrollment.workspace.0.to_string(), enrollment.host.to_string(), enrollment.agent.to_string()])?;
            Ok(Grant {
                provenance: seed.original.provenance, operation: OperationId(Ulid::new()),
                registration: seed.original.runtime.registration,
                holder: Holder { enrollment, attachment: AttachmentId(Ulid::new()), domain: TrustDomainId(domain.parse().unwrap()) },
                owning_sandbox: "daemon-selected-incarnation".into(),
            })
        }).await.unwrap();
        db.write_full("system", move |t| {
            let first = allocate_grant(t, &grant)?;
            assert_eq!(first.original.revision, 2);
            assert_eq!(first.original.runtime.registration, grant.registration);
            assert_eq!(allocate_grant(t, &grant)?, first);
            t.execute("UPDATE agents SET status = 'stopped'", [])?;
            // Returning the immutable intent is not permission to dispatch it.
            assert_eq!(allocate_grant(t, &grant)?, first);
            let mut duplicate = Grant {
                operation: OperationId(Ulid::new()),
                provenance: grant.provenance.clone(),
                registration: grant.registration,
                holder: grant.holder.clone(),
                owning_sandbox: grant.owning_sandbox.clone(),
            };
            assert!(allocate_grant(t, &duplicate).is_err());
            duplicate.holder.attachment = AttachmentId(Ulid::new());
            assert!(allocate_grant(t, &duplicate).is_err());
            let allocated: u64 = t.query_row(
                "SELECT allocated_revision FROM live_workspace_targets",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(allocated, 2);
            let count: u64 =
                t.query_row("SELECT count(*) FROM workspace_reservations", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(count, 1);
            let attachments: u64 =
                t.query_row("SELECT count(*) FROM live_workspace_attachments", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(attachments, 0);
            let revoke_id = OperationId(Ulid::new());
            let revoke = allocate_revoke(
                t,
                &grant.provenance,
                revoke_id,
                grant.registration,
                grant.holder.attachment,
            )?;
            assert_eq!(revoke.original.revision, 3);
            assert_eq!(revoke.original.holder, first.original.holder);
            assert_eq!(
                allocate_revoke(
                    t,
                    &grant.provenance,
                    revoke_id,
                    grant.registration,
                    grant.holder.attachment
                )?,
                revoke
            );
            let canceled = load(t, &grant.operation.0.to_string())?;
            assert_eq!(canceled.envelope.original, first.original);
            assert_eq!(canceled.envelope.original_hash, first.original_hash);
            assert_eq!(canceled.envelope.delivery, Delivery::Superseded);
            let tombstone: u64 = t.query_row(
                "SELECT revoke_revision FROM live_workspace_attachments",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(tombstone, 3);
            assert!(
                allocate_revoke(
                    t,
                    &grant.provenance,
                    OperationId(Ulid::new()),
                    grant.registration,
                    AttachmentId(Ulid::new())
                )
                .is_err()
            );
            assert!(
                allocate_revoke(
                    t,
                    &grant.provenance,
                    OperationId(Ulid::new()),
                    grant.registration,
                    grant.holder.attachment
                )
                .is_err()
            );
            let sealed: u64 =
                t.query_row("SELECT count(*) FROM workspace_reservations", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(sealed, 1);
            // Seed revision 1 is synthetic registration setup, not a live
            // helper operation. Advance this fixture only to exercise grants.
            t.execute("UPDATE live_workspace_operations SET phase = 'helper_committed', result = ?1 WHERE kind = 'reconcile'",
                [serde_json::to_vec(&Reply::Applied).unwrap()])?;
            t.execute("UPDATE live_workspace_operations SET phase = 'daemon_committed' WHERE kind = 'reconcile'", [])?;
            t.execute("UPDATE live_workspace_targets SET last_contiguous_revision = 1", [])?;
            assert!(commit_runtime(t, revoke_id).is_err()); // No skipping grant barrier.
            let barrier = next(t, grant.registration)?.unwrap();
            assert_eq!(barrier.envelope, canceled.envelope);
            assert_eq!(acknowledge(t, &barrier.envelope, Reply::Superseded)?, Reply::Superseded);
            commit_runtime(t, grant.operation)?;
            commit_runtime(t, grant.operation)?; // Exact runtime retry is idempotent.
            let ordered_revoke = next(t, grant.registration)?.unwrap();
            assert_eq!(ordered_revoke.envelope, revoke);
            assert_eq!(acknowledge(t, &revoke, Reply::PathRevokedVerified)?, Reply::PathRevokedVerified);
            commit_runtime(t, revoke_id)?;
            assert!(next(t, grant.registration)?.is_none());
            let state: String = t.query_row("SELECT state FROM live_workspace_attachments", [], |r| r.get(0))?;
            assert_eq!(state, "path_revoked_verified");
            t.execute("UPDATE agents SET status = 'idle'", [])?;
            let late = Grant {
                operation: OperationId(Ulid::new()), provenance: grant.provenance.clone(), registration: grant.registration,
                holder: Holder { attachment: AttachmentId(Ulid::new()), ..grant.holder.clone() },
                owning_sandbox: grant.owning_sandbox.clone(),
            };
            let late_envelope = allocate_grant(t, &late)?;
            assert_eq!(late_envelope.original.revision, 4);
            assert_eq!(acknowledge(t, &late_envelope, Reply::Applied)?, Reply::Applied);
            t.execute("UPDATE agents SET status = 'stopped'", [])?;
            commit_runtime(t, late.operation)?;
            let published: u64 = t.query_row("SELECT count(*) FROM live_workspace_attachments WHERE state = 'attached_verified'", [], |r| r.get(0))?;
            assert_eq!(published, 0);
            let cleanup = next(t, grant.registration)?.unwrap();
            assert_eq!(cleanup.envelope.original.kind, Kind::Revoke);
            assert_eq!(cleanup.envelope.original.revision, 5);
            assert_eq!(load(t, &late.operation.0.to_string())?.envelope.delivery, Delivery::Apply);
            Ok(())
        })
        .await
        .unwrap();
    }
}
