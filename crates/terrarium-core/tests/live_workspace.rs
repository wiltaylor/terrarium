use terrarium_core::live_workspace::*;
use ulid::Ulid;

fn envelope(revision: u64, kind: Kind) -> Envelope {
    let request = Request {
        version: PROTOCOL_VERSION,
        provenance: Provenance {
            daemon_instance: Ulid::from(100),
            caller: Caller::LocalOperator,
        },
        operation: OperationId(Ulid::from(revision as u128)),
        host: Ulid::from(1),
        target: "test-target".into(),
        runtime: RuntimeIdentity {
            registration: RegistrationId(Ulid::from(2)),
            incarnation: RuntimeId(Ulid::from(3)),
            container_id: "a".repeat(64),
            namespace: NamespaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        revision,
        kind,
        holder: Some(Holder {
            enrollment: Enrollment {
                workspace: WorkspaceId(Ulid::from(4)),
                host: Ulid::from(1),
                agent: terrarium_core::AgentId(Ulid::from(5)),
                project: "project".into(),
                name: "agent".into(),
                source: SourceIdentity {
                    device: 1,
                    inode: 3,
                },
            },
            attachment: AttachmentId(Ulid::from(6)),
            domain: TrustDomainId(Ulid::from(7)),
        }),
    };
    Envelope {
        original_hash: request.hash().unwrap(),
        original: request,
        delivery: Delivery::Apply,
    }
}

fn journal() -> Journal {
    let r = envelope(1, Kind::Grant).original;
    Journal {
        state_version: STATE_VERSION,
        host: r.host,
        target: r.target,
        runtime: r.runtime,
        last_contiguous_revision: 0,
        quarantined: false,
        operations: Default::default(),
        tombstones: Default::default(),
    }
}

fn finish(j: &mut Journal, e: &Envelope) -> Reply {
    let phases: &[Phase] = if e.delivery == Delivery::Superseded {
        &[Phase::Verified]
    } else {
        match e.original.kind {
            Kind::Grant => &[
                Phase::Staged,
                Phase::StageVerified,
                Phase::Published,
                Phase::Verified,
            ],
            Kind::Revoke => &[
                Phase::OrdinaryUnmountObserved,
                Phase::LazyDetachObserved,
                Phase::PlaceholderRemoved,
                Phase::Verified,
            ],
            _ => &[Phase::Verified],
        }
    };
    for &p in phases {
        j.advance(e.original.operation, p).unwrap();
    }
    j.commit(e.original.operation).unwrap()
}

#[test]
fn immutable_original_and_superseded_barrier() {
    let mut j = journal();
    let mut e = envelope(1, Kind::Grant);
    let original = e.original.clone();
    let hash = e.original_hash;
    e.delivery = Delivery::Superseded;
    assert_eq!(j.prepare(e.clone()), Ok(None));
    assert_eq!(
        j.advance(e.original.operation, Phase::Staged),
        Err(Reply::Failed)
    );
    assert_eq!(finish(&mut j, &e), Reply::Superseded);
    assert_eq!(j.prepare(e.clone()), Ok(Some(Reply::Superseded)));
    assert_eq!(e.original, original);
    assert_eq!(e.original_hash, hash);
    e.delivery = Delivery::Apply;
    assert_eq!(j.prepare(e), Err(Reply::Conflict));
    assert_eq!(j.prepare(envelope(2, Kind::Reconcile)), Ok(None));
}

#[test]
fn duplicate_gap_stale_and_pending_are_nonmutating() {
    let mut j = journal();
    let e = envelope(1, Kind::Grant);
    let before = j.clone();
    assert_eq!(
        j.prepare(envelope(2, Kind::Grant)),
        Err(Reply::RevisionGap { expected: 1 })
    );
    assert_eq!(j, before);
    j.prepare(e.clone()).unwrap();
    assert_eq!(j.prepare(e.clone()), Err(Reply::Busy));
    assert_eq!(j.commit(e.original.operation), Err(Reply::Failed));
    finish(&mut j, &e);
    assert_eq!(j.prepare(e.clone()), Ok(Some(Reply::Applied)));
    let mut divergent = e.clone();
    divergent.original.target = "different".into();
    divergent.original_hash = divergent.original.hash().unwrap();
    assert_eq!(j.prepare(divergent), Err(Reply::Unauthorized));
    let mut divergent = e.clone();
    divergent.original.kind = Kind::Reconcile;
    divergent.original_hash = divergent.original.hash().unwrap();
    assert_eq!(j.prepare(divergent), Err(Reply::Conflict));
    let mut stale = e;
    stale.original.operation = OperationId(Ulid::from(99));
    stale.original_hash = stale.original.hash().unwrap();
    assert_eq!(j.prepare(stale), Err(Reply::Stale));
}

#[test]
fn revoke_tombstone_precedes_action_and_blocks_resurrection() {
    let mut j = journal();
    let e = envelope(1, Kind::Revoke);
    j.prepare(e.clone()).unwrap();
    assert_eq!(j.tombstones.len(), 1);
    assert_eq!(finish(&mut j, &e), Reply::PathRevokedVerified);
    assert_eq!(j.prepare(envelope(2, Kind::Grant)), Err(Reply::Stale));
    let mut new = envelope(2, Kind::Grant);
    new.original.holder.as_mut().unwrap().attachment = AttachmentId(Ulid::from(88));
    new.original_hash = new.original.hash().unwrap();
    assert_eq!(j.prepare(new), Ok(None));
}

#[test]
fn every_uncommitted_crash_boundary_quarantines_without_evidence() {
    for kind in [Kind::Grant, Kind::Revoke, Kind::Register, Kind::Reconcile] {
        let mut j = journal();
        let e = envelope(1, kind);
        j.prepare(e.clone()).unwrap();
        let mut states = vec![j.clone()];
        let phases: &[Phase] = match kind {
            Kind::Grant => &[
                Phase::Staged,
                Phase::StageVerified,
                Phase::Published,
                Phase::Verified,
            ],
            Kind::Revoke => &[
                Phase::OrdinaryUnmountObserved,
                Phase::LazyDetachObserved,
                Phase::PlaceholderRemoved,
                Phase::Verified,
            ],
            _ => &[Phase::Verified],
        };
        for &p in phases {
            j.advance(e.original.operation, p).unwrap();
            states.push(j.clone());
        }
        for state in states {
            // A call before its result record has exactly the prior saved state.
            let encoded = serde_json::to_vec(&state).unwrap();
            let mut recovered: Journal = serde_json::from_slice(&encoded).unwrap();
            recovered.recover_without_evidence();
            assert!(recovered.quarantined);
            assert_eq!(recovered.last_contiguous_revision, 0);
            assert_eq!(
                recovered.commit(e.original.operation),
                Err(Reply::Quarantined)
            );
            assert_eq!(
                recovered.prepare(envelope(2, kind)),
                Err(Reply::Quarantined)
            );
        }
        j.commit(e.original.operation).unwrap();
        j.recover_without_evidence();
        assert!(!j.quarantined);
        assert!(j.prepare(e).unwrap().is_some());
    }
}

#[test]
fn strict_bounded_frames_and_disabled_activation() {
    let e = envelope(1, Kind::Grant);
    let bytes = serde_json::to_vec(&e).unwrap();
    assert_eq!(decode_frame(&bytes), Ok(e.clone()));
    assert_eq!(
        decode_frame(&vec![b' '; MAX_FRAME_BYTES + 1]),
        Err(Reply::Failed)
    );
    let mut value = serde_json::to_value(e.clone()).unwrap();
    value["original"]["source_path"] = "/".into();
    assert_eq!(
        decode_frame(&serde_json::to_vec(&value).unwrap()),
        Err(Reply::Failed)
    );
    let mut changed = e;
    changed.original.revision += 1;
    assert_eq!(
        decode_frame(&serde_json::to_vec(&changed).unwrap()),
        Err(Reply::Failed)
    );
    assert_eq!(Activation::default(), Activation::Disabled);
    assert!(serde_json::from_str::<Activation>("\"enabled\"").is_err());
}

#[test]
fn names_aliases_and_runtime_reuse_are_not_identity() {
    let e = envelope(1, Kind::Grant);
    let enrollment = &e.original.holder.as_ref().unwrap().enrollment;
    assert!(enrollment.matches(enrollment));
    let mut reused = enrollment.clone();
    reused.agent = terrarium_core::AgentId(Ulid::from(123));
    assert!(!enrollment.matches(&reused));
    reused = enrollment.clone();
    reused.name = "../agent".into();
    assert!(!enrollment.matches(&reused));
    reused = enrollment.clone();
    reused.source.inode += 1;
    assert!(!enrollment.matches(&reused));
    let mut changed = e;
    changed.original.runtime.incarnation = RuntimeId(Ulid::from(123));
    changed.original_hash = changed.original.hash().unwrap();
    assert_eq!(journal().prepare(changed), Err(Reply::Unauthorized));
}

#[test]
fn capability_is_not_activation_and_versions_fail_closed() {
    let mut c = Capability {
        version: PROTOCOL_VERSION,
        state_version: STATE_VERSION,
        workspace_mount: true,
        ready: false,
        reason: "startup reconciliation pending".into(),
    };
    assert!(!c.compatible());
    c.ready = true;
    assert!(c.compatible());
    c.state_version += 1;
    assert!(!c.compatible());
    c.state_version = STATE_VERSION;
    c.version += 1;
    assert!(!c.compatible());
    let mut j = journal();
    j.state_version += 1;
    assert_eq!(j.prepare(envelope(1, Kind::Grant)), Err(Reply::Unsupported));
}

#[test]
fn daemon_ack_and_runtime_commit_are_distinct_durable_boundaries() {
    let e = envelope(1, Kind::Grant);
    let mut op = DaemonOperation {
        envelope: e.clone(),
        phase: DaemonPhase::Intent,
        helper_result: None,
    };
    assert_eq!(op.commit_runtime(), Err(Reply::Failed));
    // Lost acknowledgement: resend exactly the saved intent, not a new revision.
    let saved = serde_json::to_vec(&op).unwrap();
    let mut restored: DaemonOperation = serde_json::from_slice(&saved).unwrap();
    assert_eq!(restored.envelope, e);
    restored.acknowledge(&e, Reply::Applied).unwrap();
    assert_eq!(restored.phase, DaemonPhase::HelperCommitted);
    restored.commit_runtime().unwrap();
    restored.acknowledge(&e, Reply::Applied).unwrap();
    assert_eq!(restored.phase, DaemonPhase::RuntimeCommitted);
    assert_eq!(
        op.acknowledge(&e, Reply::PathRevokedVerified),
        Err(Reply::Conflict)
    );
    assert_eq!(op.phase, DaemonPhase::Quarantined);
}

#[test]
fn divergent_or_missing_committed_history_quarantines() {
    let mut j = journal();
    let e = envelope(1, Kind::Grant);
    j.prepare(e.clone()).unwrap();
    finish(&mut j, &e);
    let mut missing = j.clone();
    missing.operations.clear();
    missing.recover_without_evidence();
    assert!(missing.quarantined);
    let i = j.operations.get_mut(&e.original.operation).unwrap();
    i.envelope.original_hash = [0; 32];
    j.recover_without_evidence();
    assert!(j.quarantined);
}

#[test]
fn recovery_requires_exact_revoke_tombstone_history() {
    let e = envelope(1, Kind::Revoke);
    let attachment = e.original.holder.as_ref().unwrap().attachment;
    let mut j = journal();
    j.prepare(e.clone()).unwrap();
    finish(&mut j, &e);
    let encoded = serde_json::to_vec(&j).unwrap();
    for mutation in 0..4 {
        let mut recovered: Journal = serde_json::from_slice(&encoded).unwrap();
        match mutation {
            0 => {
                recovered.tombstones.remove(&attachment);
            }
            1 => {
                recovered.tombstones.insert(attachment, 2);
            }
            2 => {
                recovered
                    .tombstones
                    .insert(AttachmentId(Ulid::from(999)), 1);
            }
            _ => {
                recovered.operations.clear();
                recovered.last_contiguous_revision = 0;
            }
        }
        // Simulate loading the altered durable journal, not just live mutation.
        let mut recovered: Journal =
            serde_json::from_slice(&serde_json::to_vec(&recovered).unwrap()).unwrap();
        recovered.recover_without_evidence();
        assert!(recovered.quarantined, "mutation {mutation}");
        assert_eq!(
            recovered.prepare(envelope(2, Kind::Grant)),
            Err(Reply::Quarantined)
        );
    }
    j.recover_without_evidence();
    assert!(!j.quarantined);
    assert_eq!(j.prepare(envelope(2, Kind::Grant)), Err(Reply::Stale));
    // Multiple revoke intents for the same attachment retain the newest barrier.
    let e2 = envelope(2, Kind::Revoke);
    j.prepare(e2.clone()).unwrap();
    finish(&mut j, &e2);
    j.recover_without_evidence();
    assert!(!j.quarantined);
    assert_eq!(j.tombstones[&attachment], 2);
}

#[test]
fn all_illegal_phase_pairs_reject_without_mutation() {
    use Phase::*;
    let phases = [
        Prepared,
        Staged,
        StageVerified,
        Published,
        OrdinaryUnmountObserved,
        LazyDetachObserved,
        PlaceholderRemoved,
        Verified,
        Committed,
    ];
    let grant_edges = [
        (Prepared, Staged),
        (Staged, StageVerified),
        (StageVerified, Published),
        (Published, Verified),
    ];
    let revoke_edges = [
        (Prepared, OrdinaryUnmountObserved),
        (OrdinaryUnmountObserved, LazyDetachObserved),
        (OrdinaryUnmountObserved, PlaceholderRemoved),
        (LazyDetachObserved, PlaceholderRemoved),
        (PlaceholderRemoved, Verified),
    ];
    for (kind, delivery, legal) in [
        (Kind::Grant, Delivery::Apply, grant_edges.as_slice()),
        (Kind::Revoke, Delivery::Apply, revoke_edges.as_slice()),
        (
            Kind::Grant,
            Delivery::Superseded,
            &[(Prepared, Verified)][..],
        ),
    ] {
        for &from in &phases {
            for &to in &phases {
                let mut j = journal();
                let mut e = envelope(1, kind);
                e.delivery = delivery;
                j.prepare(e.clone()).unwrap();
                j.operations.get_mut(&e.original.operation).unwrap().phase = from;
                let before = j.clone();
                let result = j.advance(e.original.operation, to);
                if legal.contains(&(from, to)) {
                    assert_eq!(result, Ok(()), "{kind:?}/{delivery:?}: {from:?} -> {to:?}");
                    assert_eq!(j.operations[&e.original.operation].phase, to);
                } else {
                    assert_eq!(
                        result,
                        Err(Reply::Failed),
                        "{kind:?}/{delivery:?}: {from:?} -> {to:?}"
                    );
                    assert_eq!(j, before, "refused transition mutated journal");
                }
            }
        }
    }
}
