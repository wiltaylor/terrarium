//! B1 schema tests only. No production admission or privileged operations.
use rusqlite::{Connection, TransactionBehavior, params};
use std::sync::{Arc, Barrier};

const SCHEMA: &str = include_str!("../../migrations/ter-924-live-workspace.sql");

#[test]
fn daemon_connections_enable_recursive_triggers() {
    let dir = tempfile::tempdir().unwrap();
    let c = terrariumd::db::open(&dir.path().join("daemon.sqlite")).unwrap();
    assert_eq!(
        c.pragma_query_value(None, "recursive_triggers", |r| r.get::<_, u32>(0))
            .unwrap(),
        1
    );
}

fn snapshot(c: &Connection) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    [
        "live_workspaces",
        "workspace_reservations",
        "live_workspace_targets",
        "live_workspace_attachments",
        "live_workspace_operations",
    ]
    .iter()
    .map(|table| {
        let mut statement = c
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| (0..columns).map(|n| row.get(n)).collect())
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    })
    .collect()
}

#[test]
fn replace_and_upsert_cannot_erase_seals_or_history() {
    for recursive in [false, true] {
        let c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", true).unwrap();
        c.pragma_update(None, "recursive_triggers", recursive)
            .unwrap();
        seed(&c);
        c.execute_batch("INSERT INTO workspace_reservations VALUES ('w', 'domain', 'r1', 'sandbox');
            INSERT INTO live_workspace_attachments VALUES ('attachment', 'w', 'r1', 'agent-ulid', 'attached_verified', NULL);
            INSERT INTO live_workspace_operations VALUES ('op', 'r1', 1, 'grant', X'01', zeroblob(32), 'apply', 'intent', NULL);").unwrap();
        for revoked in [false, true] {
            if revoked {
                c.execute_batch("UPDATE live_workspace_attachments SET state = 'authority_ended', revoke_revision = 2 WHERE attachment_id = 'attachment';
                    UPDATE live_workspace_attachments SET state = 'revoke_pending' WHERE attachment_id = 'attachment';
                    UPDATE live_workspace_attachments SET state = 'path_revoked_verified' WHERE attachment_id = 'attachment';
                    UPDATE workspace_reservations SET current_registration_id = NULL, owning_sandbox_incarnation = NULL WHERE workspace_id = 'w';
                    UPDATE live_workspace_targets SET state = 'destroyed' WHERE registration_id = 'r1';
                    UPDATE live_workspace_operations SET phase = 'helper_committed', result = X'02' WHERE operation_id = 'op';
                    UPDATE live_workspace_operations SET phase = 'daemon_committed' WHERE operation_id = 'op';").unwrap();
            }
            let before = snapshot(&c);
            // UPDATE must not move an immutable identity out of the way of a
            // subsequent INSERT, including through alternate identity columns.
            for (table, key, value, columns, message) in [
                ("live_workspaces", "workspace_id", "w",
                    &["workspace_id", "host_id", "agent_id", "project_hint", "agent_hint", "source_dev", "source_ino", "history"][..], "enrollment is immutable"),
                ("workspace_reservations", "workspace_id", "w",
                    &["workspace_id", "trust_domain_id"][..], "workspace domain is permanently sealed"),
                ("live_workspace_targets", "registration_id", "r1",
                    &["registration_id", "host_id", "target_id", "trust_domain_id", "runtime_id", "container_id", "namespace_dev", "namespace_ino"][..], "registration identity is immutable"),
                ("live_workspace_attachments", "attachment_id", "attachment",
                    &["attachment_id", "workspace_id", "registration_id", "agent_id"][..], "attachment identity is immutable"),
                ("live_workspace_operations", "operation_id", "op",
                    &["operation_id", "registration_id", "revision", "kind", "original_request", "original_hash"][..], "original operation is immutable"),
            ] {
                for column in columns {
                    let sql = format!("UPDATE {table} SET {column} = 'renamed' WHERE {key} = ?1");
                    let error = c.execute(&sql, [value]).unwrap_err();
                    assert!(error.to_string().contains(message), "identity guard did not reject {sql}: {error}");
                    assert_eq!(snapshot(&c), before, "identity update changed history: {sql}");
                    let sql = format!("INSERT INTO {table} SELECT * FROM {table} WHERE {key} = ?1");
                    assert!(c.execute(&sql, [value]).is_err());
                    assert_eq!(snapshot(&c), before, "reinsert changed history: {sql}");
                }
            }
            for sql in [
                "INSERT OR REPLACE INTO workspace_reservations VALUES ('w', 'foreign', NULL, NULL)",
                "INSERT INTO workspace_reservations VALUES ('w', 'foreign', NULL, NULL) ON CONFLICT(workspace_id) DO UPDATE SET trust_domain_id = excluded.trust_domain_id",
                "INSERT OR REPLACE INTO live_workspaces VALUES ('w', 'h', 'different-agent', 'project', 'name', '1', '99', 'fresh')",
                "INSERT INTO live_workspaces VALUES ('w', 'h', 'different-agent', 'project', 'name', '1', '99', 'fresh') ON CONFLICT(workspace_id) DO UPDATE SET agent_id = excluded.agent_id",
                // Alternate unique keys must not implicitly delete enrollment.
                "INSERT OR REPLACE INTO live_workspaces VALUES ('new', 'h', 'agent-ulid', 'project', 'name', '1', '99', 'fresh')",
                "INSERT OR REPLACE INTO live_workspaces VALUES ('new', 'h', 'different-agent', 'project', 'name', '1', '2', 'fresh')",
                "INSERT OR REPLACE INTO live_workspace_attachments VALUES ('attachment', 'w', 'r1', 'agent-ulid', 'attached_verified', NULL)",
                "INSERT INTO live_workspace_attachments VALUES ('attachment', 'w', 'r1', 'agent-ulid', 'attached_verified', NULL) ON CONFLICT(attachment_id) DO UPDATE SET revoke_revision = excluded.revoke_revision",
                "INSERT OR REPLACE INTO live_workspace_operations VALUES ('op', 'r1', 1, 'grant', X'FF', zeroblob(32), 'apply', 'intent', NULL)",
                "INSERT INTO live_workspace_operations VALUES ('op', 'r1', 1, 'grant', X'FF', zeroblob(32), 'apply', 'intent', NULL) ON CONFLICT(operation_id) DO UPDATE SET original_request = excluded.original_request",
                "INSERT OR REPLACE INTO live_workspace_operations VALUES ('different', 'r1', 1, 'grant', X'FF', zeroblob(32), 'apply', 'intent', NULL)",
                "INSERT INTO live_workspace_operations VALUES ('different', 'r1', 1, 'grant', X'FF', zeroblob(32), 'apply', 'intent', NULL) ON CONFLICT(registration_id, revision) DO UPDATE SET operation_id = excluded.operation_id",
                "INSERT OR REPLACE INTO live_workspace_targets SELECT registration_id, host_id, target_id, 'foreign', runtime_id, container_id, namespace_dev, namespace_ino, activation, state, last_contiguous_revision, allocated_revision FROM live_workspace_targets WHERE registration_id = 'r1'",
                "INSERT INTO live_workspace_targets SELECT * FROM live_workspace_targets WHERE registration_id = 'r1' ON CONFLICT(registration_id) DO UPDATE SET trust_domain_id = 'foreign'",
            ] {
                assert!(
                    c.execute_batch(sql).is_err(),
                    "recursive={recursive}, revoked={revoked}: {sql}"
                );
                assert_eq!(snapshot(&c), before, "failed insert changed history: {sql}");
            }
        }
    }
}

#[test]
fn stored_state_transitions_cannot_skip_reconcile_or_revoke() {
    let target_states = ["quarantined", "reconcile_pending", "verified", "destroyed"];
    let attachment_states = ["attached_verified", "authority_ended", "revoke_pending", "path_revoked_verified", "quarantined"];
    let target_edges = [("quarantined", "reconcile_pending"), ("quarantined", "destroyed"),
        ("reconcile_pending", "verified"), ("reconcile_pending", "quarantined"), ("reconcile_pending", "destroyed"),
        ("verified", "quarantined"), ("verified", "reconcile_pending"), ("verified", "destroyed")];
    let attachment_edges = [("attached_verified", "authority_ended"), ("attached_verified", "quarantined"),
        ("authority_ended", "revoke_pending"), ("authority_ended", "quarantined"),
        ("revoke_pending", "path_revoked_verified"), ("revoke_pending", "quarantined"), ("quarantined", "revoke_pending")];
    for (table, states, legal) in [
        ("live_workspace_targets", target_states.as_slice(), target_edges.as_slice()),
        ("live_workspace_attachments", attachment_states.as_slice(), attachment_edges.as_slice()),
    ] {
        for &from in states {
            for &to in states {
                let c = Connection::open_in_memory().unwrap();
                seed(&c);
                c.execute("INSERT INTO workspace_reservations VALUES ('w', 'domain', 'r1', 'sandbox')", []).unwrap();
                let key = if table == "live_workspace_targets" {
                    c.execute("INSERT INTO live_workspace_targets
                        (registration_id, host_id, target_id, trust_domain_id, runtime_id, container_id, namespace_dev, namespace_ino, state)
                        VALUES ('subject', 'h', 'subject', 'domain', 'runtime', ?1, '3', '4', ?2)", params!["a".repeat(64), from]).unwrap();
                    "registration_id"
                } else {
                    let tombstone = if from == "attached_verified" { None } else { Some(1) };
                    c.execute("INSERT INTO live_workspace_attachments VALUES ('subject', 'w', 'r1', 'agent-ulid', ?1, ?2)", params![from, tombstone]).unwrap();
                    "attachment_id"
                };
                let before = snapshot(&c);
                let result = c.execute(&format!("UPDATE {table} SET state = ?1 WHERE {key} = 'subject'"), [to]);
                if from == to || legal.contains(&(from, to)) {
                    assert!(result.is_ok(), "{table}: {from} -> {to}: {result:?}");
                } else {
                    assert!(result.is_err(), "{table}: {from} -> {to}");
                    assert_eq!(snapshot(&c), before, "invalid state transition mutated history");
                }
            }
        }
    }
}

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
    c.execute(
        "UPDATE live_workspace_operations SET phase = 'daemon_committed' WHERE operation_id = 'op'",
        [],
    )
    .unwrap();
    assert!(
        c.execute(
            "UPDATE live_workspace_operations SET phase = 'intent' WHERE operation_id = 'op'",
            []
        )
        .is_err()
    );
    assert!(
        c.execute(
            "UPDATE live_workspace_operations SET result = X'02' WHERE operation_id = 'op'",
            []
        )
        .is_err()
    );
    assert!(
        c.execute(
            "UPDATE live_workspace_targets SET allocated_revision = 2 WHERE registration_id = 'r1'",
            []
        )
        .is_err()
    );
    c.execute(
        "UPDATE live_workspace_targets SET allocated_revision = 1 WHERE registration_id = 'r1'",
        [],
    )
    .unwrap();
    c.execute("UPDATE live_workspace_targets SET last_contiguous_revision = 1 WHERE registration_id = 'r1'", []).unwrap();
    assert!(c.execute("UPDATE live_workspace_targets SET last_contiguous_revision = 0 WHERE registration_id = 'r1'", []).is_err());
}
