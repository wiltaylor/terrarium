//! Ordering/model tests, not privileged bootstrap evidence.
use terrarium_core::live_workspace::bootstrap::{Journal, Request, Scaffold, Step};
use terrarium_core::live_workspace::{
    Caller, NamespaceIdentity, OperationId, PROTOCOL_VERSION, Provenance, RegistrationId,
    RuntimeId, RuntimeIdentity, SourceIdentity, TrustDomainId,
};
use ulid::Ulid;

fn request() -> Request {
    Request {
        version: PROTOCOL_VERSION,
        operation: OperationId(Ulid::new()),
        provenance: Provenance {
            daemon_instance: Ulid::new(),
            caller: Caller::LocalOperator,
        },
        host: Ulid::new(),
        target: "fixture".into(),
        registration: RegistrationId(Ulid::new()),
        incarnation: RuntimeId(Ulid::new()),
        domain: TrustDomainId(Ulid::new()),
        configuration: [1; 32],
    }
}

#[test]
fn normal_journal_requires_exact_committed_bootstrap_ownership() {
    let r = request();
    let all = steps(&r);
    let Step::ReceiverVerified(runtime) = &all[4] else {
        panic!("fixture")
    };
    let normal = terrarium_core::live_workspace::Journal {
        state_version: terrarium_core::live_workspace::STATE_VERSION,
        host: r.host,
        target: r.target.clone(),
        runtime: runtime.clone(),
        last_contiguous_revision: 0,
        quarantined: false,
        operations: Default::default(),
        tombstones: Default::default(),
    };
    let mut bootstrap = Journal::new(r).unwrap();
    for step in &all[1..] {
        assert!(bootstrap.runtime_ownership(&normal).is_err());
        bootstrap.advance(step.clone()).unwrap();
    }
    assert!(bootstrap.runtime_ownership(&normal).is_ok());
    for field in 0..5 {
        let mut changed = normal.clone();
        match field {
            0 => changed.host = Ulid::new(),
            1 => changed.target.push_str("foreign"),
            2 => changed.runtime.container_id = "b".repeat(64),
            3 => changed.runtime.namespace.inode += 1,
            _ => changed.state_version += 1,
        }
        assert!(bootstrap.runtime_ownership(&changed).is_err());
    }
    bootstrap.quarantine();
    assert!(bootstrap.runtime_ownership(&normal).is_err());
}

fn steps(r: &Request) -> Vec<Step> {
    vec![
        Step::Intent,
        Step::ScaffoldVerified(Scaffold {
            publisher: SourceIdentity {
                device: 1,
                inode: 1,
            },
            staging: SourceIdentity {
                device: 1,
                inode: 2,
            },
            state: SourceIdentity {
                device: 1,
                inode: 3,
            },
        }),
        Step::CreatePending,
        Step::ContainerBound("a".repeat(64)),
        Step::ReceiverVerified(RuntimeIdentity {
            registration: r.registration,
            incarnation: r.incarnation,
            container_id: "a".repeat(64),
            namespace: NamespaceIdentity {
                device: 2,
                inode: 8,
            },
        }),
        Step::Committed,
    ]
}

#[test]
fn missing_and_zero_configuration_digests_are_not_bootstrap_requests() {
    let r = request();
    let mut value = serde_json::to_value(&r).unwrap();
    value.as_object_mut().unwrap().remove("configuration");
    assert!(serde_json::from_value::<Request>(value).is_err());
    let mut zero = r;
    zero.configuration = [0; 32];
    assert!(Journal::new(zero).is_err());
}

#[test]
fn every_skipped_repeated_and_reordered_step_is_refused() {
    let r = request();
    let all = steps(&r);
    for at in 0..all.len() {
        let mut j = Journal::new(r.clone()).unwrap();
        for step in &all[1..=at] {
            j.advance(step.clone()).unwrap();
        }
        for (next, step) in all.iter().enumerate() {
            let mut candidate = j.clone();
            assert_eq!(candidate.advance(step.clone()).is_ok(), next == at + 1);
            if next != at + 1 {
                assert_eq!(candidate, j);
            }
        }
    }
}

#[test]
fn every_uncommitted_crash_boundary_quarantines() {
    let r = request();
    let all = steps(&r);
    let mut j = Journal::new(r.clone()).unwrap();
    for at in 0..all.len() {
        let bytes = serde_json::to_vec(&j).unwrap();
        let mut recovered: Journal = serde_json::from_slice(&bytes).unwrap();
        recovered.recover_without_evidence();
        assert_eq!(recovered.quarantined(), at != all.len() - 1);
        assert_eq!(recovered.retry(&r).is_ok(), at == all.len() - 1);
        if let Some(next) = all.get(at + 1) {
            assert!(recovered.advance(next.clone()).is_err());
            j.advance(next.clone()).unwrap();
        }
    }
}

#[test]
fn immutable_request_and_bound_runtime_cannot_be_substituted() {
    let r = request();
    let all = steps(&r);
    let mut j = Journal::new(r.clone()).unwrap();
    for step in &all[1..4] {
        j.advance(step.clone()).unwrap();
    }
    let Step::ReceiverVerified(runtime) = &all[4] else {
        panic!()
    };
    for field in 0..4 {
        let mut wrong = runtime.clone();
        match field {
            0 => wrong.registration = RegistrationId(Ulid::new()),
            1 => wrong.incarnation = RuntimeId(Ulid::new()),
            2 => wrong.container_id = "b".repeat(64),
            _ => wrong.namespace.inode = 0,
        }
        assert!(j.advance(Step::ReceiverVerified(wrong)).is_err());
    }
    j.advance(all[4].clone()).unwrap();
    j.advance(Step::Committed).unwrap();
    assert!(j.retry(&r).is_ok());
    for field in 0..8 {
        let mut changed = r.clone();
        match field {
            0 => changed.operation = OperationId(Ulid::new()),
            1 => changed.host = Ulid::new(),
            2 => changed.target = "other".into(),
            3 => changed.registration = RegistrationId(Ulid::new()),
            4 => changed.incarnation = RuntimeId(Ulid::new()),
            5 => changed.domain = TrustDomainId(Ulid::new()),
            6 => changed.provenance.caller = Caller::Link(Ulid::new()),
            _ => changed.configuration = [2; 32],
        }
        assert!(j.retry(&changed).is_err());
    }
}

#[test]
fn corrupt_history_never_recovers_as_committed() {
    let r = request();
    let mut j = Journal::new(r.clone()).unwrap();
    for step in &steps(&r)[1..] {
        j.advance(step.clone()).unwrap();
    }
    let saved = serde_json::to_value(j).unwrap();
    for field in ["steps", "original_hash", "original"] {
        let mut corrupt = saved.clone();
        match field {
            "steps" => {
                corrupt["steps"].as_array_mut().unwrap().remove(2);
            }
            "original_hash" => {
                corrupt[field][0] = 256.into();
            }
            _ => {
                corrupt[field]["target"] = "different".into();
            }
        }
        if let Ok(mut recovered) = serde_json::from_value::<Journal>(corrupt) {
            recovered.recover_without_evidence();
            assert!(recovered.quarantined());
            assert!(recovered.retry(&r).is_err());
        }
    }
}

#[test]
fn link_provenance_aliases_and_untyped_paths_are_refused() {
    let mut r = request();
    r.provenance.caller = Caller::Link(Ulid::new());
    assert!(Journal::new(r).is_err());
    let mut r = request();
    r.target = "../other".into();
    assert!(Journal::new(r).is_err());
    for field in [
        "publisher_root",
        "state_root",
        "staging_root",
        "pid",
        "flags",
    ] {
        let mut wire = serde_json::to_value(request()).unwrap();
        wire[field] = "/arbitrary".into();
        assert!(serde_json::from_value::<Request>(wire).is_err());
    }
}

#[test]
fn scaffold_aliases_zero_identity_and_short_container_ids_are_refused() {
    let r = request();
    let all = steps(&r);
    let Step::ScaffoldVerified(scaffold) = &all[1] else {
        panic!()
    };
    for field in 0..4 {
        let mut wrong = scaffold.clone();
        match field {
            0 => wrong.state = wrong.publisher.clone(),
            1 => wrong.staging = wrong.publisher.clone(),
            2 => wrong.state = wrong.staging.clone(),
            _ => wrong.publisher.inode = 0,
        }
        assert!(
            Journal::new(r.clone())
                .unwrap()
                .advance(Step::ScaffoldVerified(wrong))
                .is_err()
        );
    }
    let mut j = Journal::new(r).unwrap();
    j.advance(all[1].clone()).unwrap();
    j.advance(Step::CreatePending).unwrap();
    for id in ["", "name", &"g".repeat(64), &"a".repeat(63)] {
        assert!(j.advance(Step::ContainerBound(id.into())).is_err());
    }
}
