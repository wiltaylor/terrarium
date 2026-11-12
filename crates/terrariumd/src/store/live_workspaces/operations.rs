//! Durable receipt ingestion only. This does not dispatch, enable a target,
//! advance the runtime barrier or claim that an attachment is usable.
use super::*;
use terrarium_core::live_workspace::{
    Caller, DaemonOperation, DaemonPhase, Delivery, Envelope, Reply, Request, decode_frame,
};

fn conflict() -> ApiError {
    ApiError::new(Code::Conflict, "invalid live workspace operation record")
}

#[allow(clippy::type_complexity)]
fn load(t: &Tx<'_>, operation: &str) -> ApiResult<DaemonOperation> {
    let (registration, revision, kind, bytes, hash, delivery, phase, result): (
        String, u64, String, Vec<u8>, Vec<u8>, String, String, Option<Vec<u8>>,
    ) = t.query_row(
        "SELECT registration_id, revision, kind, original_request, original_hash,
         delivery, phase, result FROM live_workspace_operations WHERE operation_id = ?1",
        [operation],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
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
    if matches!(phase, DaemonPhase::HelperCommitted | DaemonPhase::RuntimeCommitted)
        && helper_result.is_none()
    {
        return Err(conflict());
    }
    Ok(DaemonOperation { envelope, phase, helper_result })
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
        [&operation], |r| r.get(0),
    )?;
    let valid = (|| -> ApiResult<DaemonOperation> {
        let mut stored = load(t, &operation)?;
        let r = &stored.envelope.original;
        let (host, target, runtime, container, dev, ino, state, last): (
            String, String, String, String, String, String, String, u64,
        ) = t.query_row(
            "SELECT host_id, target_id, runtime_id, container_id, namespace_dev,
             namespace_ino, state, last_contiguous_revision FROM live_workspace_targets
             WHERE registration_id = ?1", [&registration],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
        )?;
        let next = last.checked_add(1).ok_or_else(conflict)?;
        if host != r.host.to_string() || target != r.target
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
        stored.acknowledge(receipt, result).map_err(|_| conflict())?;
        Ok(stored)
    })();
    if valid.is_err() {
        t.execute("UPDATE live_workspace_operations SET phase = 'quarantined' WHERE operation_id = ?1", [&operation])?;
        t.execute("UPDATE live_workspace_targets SET state = 'quarantined' WHERE registration_id = ?1 AND state != 'destroyed'", [&registration])?;
        return Ok(Reply::Quarantined);
    }
    let stored = valid?;
    if stored.phase == DaemonPhase::HelperCommitted {
        let bytes = serde_json::to_vec(&result).map_err(|_| conflict())?;
        t.execute("UPDATE live_workspace_operations SET phase = 'helper_committed', result = ?2 WHERE operation_id = ?1 AND phase = 'intent'",
            params![operation, bytes])?;
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
        Kind, NamespaceIdentity, OperationId, Provenance, RegistrationId, RuntimeId,
        RuntimeIdentity, PROTOCOL_VERSION,
    };

    fn insert(t: &Tx<'_>) -> ApiResult<Envelope> {
        let r = Request {
            version: PROTOCOL_VERSION,
            provenance: Provenance { daemon_instance: Ulid::new(), caller: Caller::LocalOperator },
            operation: OperationId(Ulid::new()), host: Ulid::new(), target: "synthetic".into(),
            runtime: RuntimeIdentity {
                registration: RegistrationId(Ulid::new()), incarnation: RuntimeId(Ulid::new()),
                container_id: "a".repeat(64), namespace: NamespaceIdentity { device: 1, inode: 2 },
            }, revision: 1, kind: Kind::Reconcile, holder: None,
        };
        let registration = r.runtime.registration.0.to_string();
        t.execute("INSERT INTO live_workspace_targets (registration_id,host_id,target_id,trust_domain_id,runtime_id,container_id,namespace_dev,namespace_ino)
            VALUES (?1,?2,?3,?4,?5,?6,'1','2')",
            params![registration, r.host.to_string(), r.target, Ulid::new().to_string(), r.runtime.incarnation.0.to_string(), r.runtime.container_id])?;
        t.execute("UPDATE live_workspace_targets SET state = 'reconcile_pending' WHERE registration_id = ?1", [&registration])?;
        t.execute("UPDATE live_workspace_targets SET state = 'verified', allocated_revision = 1 WHERE registration_id = ?1", [&registration])?;
        let envelope = Envelope { original_hash: r.hash().unwrap(), original: r, delivery: Delivery::Apply };
        t.execute("INSERT INTO live_workspace_operations VALUES (?1,?2,1,'reconcile',?3,?4,'apply','intent',NULL)",
            params![envelope.original.operation.0.to_string(), registration, serde_json::to_vec(&envelope.original).unwrap(), envelope.original_hash.as_slice()])?;
        Ok(envelope)
    }

    #[tokio::test]
    async fn exact_receipt_is_durable_but_never_commits_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(crate::db::open(&dir.path().join("test.sqlite")).unwrap(), EventHub::default());
        let receipt = db.write_full("system", insert).await.unwrap();
        let check = receipt.clone();
        db.write_full("system", move |t| {
            assert_eq!(acknowledge(t, &check, Reply::Applied)?, Reply::Applied);
            assert_eq!(acknowledge(t, &check, Reply::Applied)?, Reply::Applied);
            let record = load(t, &check.original.operation.0.to_string())?;
            assert_eq!(record.phase, DaemonPhase::HelperCommitted);
            let last: u64 = t.query_row("SELECT last_contiguous_revision FROM live_workspace_targets", [], |r| r.get(0))?;
            assert_eq!(last, 0);
            Ok(())
        }).await.unwrap();
        db.write_full("system", move |t| {
            let saved = load(t, &receipt.original.operation.0.to_string())?;
            assert_eq!(saved.helper_result, Some(Reply::Applied));
            assert_eq!(saved.phase, DaemonPhase::HelperCommitted);
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn receipt_substitution_fences_durable_owner_not_supplied_registration() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(crate::db::open(&dir.path().join("test.sqlite")).unwrap(), EventHub::default());
        let mut receipt = db.write_full("system", insert).await.unwrap();
        let operation = receipt.original.operation.0.to_string();
        receipt.original.runtime.registration = RegistrationId(Ulid::new());
        db.write_full("system", move |t| {
            assert_eq!(acknowledge(t, &receipt, Reply::Applied)?, Reply::Quarantined);
            Ok(()) // Commit the fence rather than returning an error.
        }).await.unwrap();
        db.write_full("system", move |t| {
            assert_eq!(load(t, &operation)?.phase, DaemonPhase::Quarantined);
            let state: String = t.query_row("SELECT state FROM live_workspace_targets", [], |r| r.get(0))?;
            assert_eq!(state, "quarantined");
            Ok(())
        }).await.unwrap();
    }
}
