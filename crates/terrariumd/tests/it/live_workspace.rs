//! B1 schema tests only. No production admission or privileged operations.
use rusqlite::{Connection, TransactionBehavior, params};
use std::sync::{Arc, Barrier};

const SCHEMA: &str = include_str!("../../migrations/ter-924-live-workspace.sql");

fn open(path: &std::path::Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c.pragma_update(None, "foreign_keys", true).unwrap();
    c.pragma_update(None, "synchronous", "FULL").unwrap();
    c
}

fn seed(c: &Connection) {
    c.execute_batch(SCHEMA).unwrap();
    c.execute("INSERT INTO live_workspaces VALUES ('w', 'h', 'agent-ulid', 'project', 'name', '1', '2', 'fresh')", []).unwrap();
    for registration in ["r1", "r2"] {
        c.execute("INSERT INTO live_workspace_targets
            (registration_id, host_id, target_id, trust_domain_id, runtime_id, container_id, namespace_dev, namespace_ino)
            VALUES (?1, 'h', ?1, 'domain', 'runtime', ?2, '3', '4')",
            params![registration, "a".repeat(64)]).unwrap();
    }
}

#[test]
fn concurrent_immediate_reservations_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reservations.sqlite");
    let c = open(&path);
    seed(&c);
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for registration in ["r1", "r2"] {
        let path = path.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            let mut c = open(&path);
            barrier.wait();
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let won = tx
                .execute(
                    "INSERT INTO workspace_reservations VALUES ('w', 'domain', ?1, 'sandbox')",
                    [registration],
                )
                .is_ok();
            tx.commit().unwrap();
            won
        }));
    }
    let winners = handles
        .into_iter()
        .map(|h| usize::from(h.join().unwrap()))
        .sum::<usize>();
    assert_eq!(winners, 1);
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM workspace_reservations WHERE workspace_id = 'w'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        1
    );
    assert!(c.execute("UPDATE workspace_reservations SET current_registration_id = CASE current_registration_id WHEN 'r1' THEN 'r2' ELSE 'r1' END WHERE workspace_id = 'w'", []).is_err());
}

#[test]
fn revoke_and_target_destruction_never_release_domain_seal() {
    let c = Connection::open_in_memory().unwrap();
    c.pragma_update(None, "foreign_keys", true).unwrap();
    seed(&c);
    c.execute_batch("INSERT INTO workspace_reservations VALUES ('w', 'domain', 'r1', 'sandbox');
        INSERT INTO live_workspace_attachments VALUES ('attachment', 'w', 'r1', 'agent-ulid', 'revoke_pending', 1);
        UPDATE live_workspace_attachments SET state = 'path_revoked_verified' WHERE attachment_id = 'attachment';
        UPDATE workspace_reservations SET current_registration_id = NULL, owning_sandbox_incarnation = NULL WHERE workspace_id = 'w';
        UPDATE live_workspace_targets SET state = 'destroyed' WHERE registration_id = 'r1';").unwrap();
    assert!(
        c.execute(
            "DELETE FROM workspace_reservations WHERE workspace_id = 'w'",
            []
        )
        .is_err()
    );
    assert!(c.execute("UPDATE workspace_reservations SET trust_domain_id = 'foreign' WHERE workspace_id = 'w'", []).is_err());
    assert!(c.execute("UPDATE live_workspace_attachments SET revoke_revision = NULL WHERE attachment_id = 'attachment'", []).is_err());
    assert!(c.execute("UPDATE live_workspace_attachments SET state = 'attached_verified' WHERE attachment_id = 'attachment'", []).is_err());
    assert!(
        c.execute("DELETE FROM live_workspaces WHERE workspace_id = 'w'", [])
            .is_err()
    );
    assert!(
        c.execute(
            "UPDATE live_workspace_targets SET activation = 'enabled' WHERE registration_id = 'r2'",
            []
        )
        .is_err()
    );
    assert!(c.execute("UPDATE live_workspace_targets SET trust_domain_id = 'foreign' WHERE registration_id = 'r2'", []).is_err());
}

#[test]
fn enrollment_refuses_alias_replacement_reused_agent_and_unknown_history() {
    let c = Connection::open_in_memory().unwrap();
    seed(&c);
    // Same opened inode via another path or another agent is still not fresh.
    assert!(c.execute("INSERT INTO live_workspaces VALUES ('alias', 'h', 'other-agent', 'other', 'alias', '1', '2', 'fresh')", []).is_err());
    assert!(c.execute("INSERT INTO live_workspaces VALUES ('replacement', 'h', 'agent-ulid', 'project', 'name', '1', '99', 'fresh')", []).is_err());
    c.execute("INSERT INTO live_workspaces VALUES ('new-agent', 'h', 'new-ulid', 'project', 'name', '1', '3', 'unknown')", []).unwrap();
    assert!(
        c.execute(
            "INSERT INTO workspace_reservations VALUES ('new-agent', 'domain', 'r1', NULL)",
            []
        )
        .is_err()
    );
    assert!(
        c.execute(
            "UPDATE live_workspaces SET history = 'fresh' WHERE workspace_id = 'new-agent'",
            []
        )
        .is_err()
    );
}

#[test]
fn operation_original_is_immutable_and_delivery_is_separate() {
    let c = Connection::open_in_memory().unwrap();
    seed(&c);
    c.execute("INSERT INTO live_workspace_operations VALUES ('op', 'r1', 1, 'grant', ?1, ?2, 'apply', 'intent', NULL)", params![b"original".as_slice(), [1u8; 32].as_slice()]).unwrap();
    assert!(c.execute("UPDATE live_workspace_operations SET original_request = X'00' WHERE operation_id = 'op'", []).is_err());
    assert!(
        c.execute(
            "UPDATE live_workspace_operations SET original_hash = ?1 WHERE operation_id = 'op'",
            params![[2u8; 32].as_slice()]
        )
        .is_err()
    );
    c.execute(
        "UPDATE live_workspace_operations SET delivery = 'superseded' WHERE operation_id = 'op'",
        [],
    )
    .unwrap();
    assert!(
        c.execute(
            "UPDATE live_workspace_operations SET delivery = 'apply' WHERE operation_id = 'op'",
            []
        )
        .is_err()
    );
    assert!(c.execute("INSERT INTO live_workspace_operations SELECT 'other', registration_id, revision, kind, original_request, original_hash, delivery, phase, result FROM live_workspace_operations WHERE operation_id = 'op'", []).is_err());
    assert!(c.execute("UPDATE live_workspace_operations SET phase = 'daemon_committed', result = X'01' WHERE operation_id = 'op'", []).is_err());
    c.execute("UPDATE live_workspace_operations SET phase = 'helper_committed', result = X'01' WHERE operation_id = 'op'", []).unwrap();
    c.execute("UPDATE live_workspace_operations SET phase = 'daemon_committed' WHERE operation_id = 'op'", []).unwrap();
    assert!(c.execute("UPDATE live_workspace_operations SET phase = 'intent' WHERE operation_id = 'op'", []).is_err());
    assert!(c.execute("UPDATE live_workspace_operations SET result = X'02' WHERE operation_id = 'op'", []).is_err());
    assert!(c.execute("UPDATE live_workspace_targets SET allocated_revision = 2 WHERE registration_id = 'r1'", []).is_err());
    c.execute("UPDATE live_workspace_targets SET allocated_revision = 1 WHERE registration_id = 'r1'", []).unwrap();
    c.execute("UPDATE live_workspace_targets SET last_contiguous_revision = 1 WHERE registration_id = 'r1'", []).unwrap();
    assert!(c.execute("UPDATE live_workspace_targets SET last_contiguous_revision = 0 WHERE registration_id = 'r1'", []).is_err());
}
