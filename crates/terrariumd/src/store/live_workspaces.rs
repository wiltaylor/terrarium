//! Durable reservation checks. These primitives do not enroll workspaces,
//! dispatch helper operations or enable the production gate.
use crate::db::Tx;
use crate::error::{ApiError, ApiResult, Code};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use terrarium_core::live_workspace::Enrollment;

/// Authority is selected by daemon code, never deserialized from a request.
pub enum WorkspaceUse<'a> {
    OwnerSandbox {
        incarnation: &'a str,
    },
    Live {
        registration: &'a str,
        domain: &'a str,
    },
    Other,
}

/// Unlike operator_only, this has no remote dashboard route exceptions.
pub fn no_link(t: &Tx<'_>) -> ApiResult<()> {
    if t.link.is_some() || crate::links::is_link_call() || t.actor.starts_with("link:") {
        return Err(ApiError::new(
            Code::LinkForbidden,
            "link keys may not change or use live workspace authority",
        ));
    }
    Ok(())
}

fn durable(t: &Tx<'_>) -> ApiResult<()> {
    let mode: u32 = t.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    if mode < 2 {
        return Err(ApiError::internal(
            "live workspace intent requires FULL synchronous durability",
        ));
    }
    Ok(())
}

fn identity(t: &Tx<'_>, e: &Enrollment) -> ApiResult<String> {
    let row = t
        .query_row(
            "SELECT host_id, agent_id, project_hint, agent_hint, source_dev, source_ino, history
         FROM live_workspaces WHERE workspace_id = ?1",
            [e.workspace.0.to_string()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((host, agent, project, name, dev, ino, history)) = row else {
        return Err(ApiError::new(
            Code::Conflict,
            "workspace has no verified enrollment",
        ));
    };
    if host != e.host.to_string()
        || agent != e.agent.to_string()
        || project != e.project
        || name != e.name
        || dev != e.source.device.to_string()
        || ino != e.source.inode.to_string()
        || e.source.inode == 0
        || !e.matches(e)
    {
        return Err(ApiError::new(
            Code::Conflict,
            "workspace enrollment identity mismatch",
        ));
    }
    Ok(history)
}

fn current_authority(t: &Tx<'_>, e: &Enrollment, registration: &str) -> ApiResult<bool> {
    let target: String = t.query_row(
        "SELECT target_id FROM live_workspace_targets WHERE registration_id = ?1",
        [registration],
        |r| r.get(0),
    )?;
    let agent: Option<(String, String, bool)> = t
        .query_row(
            "SELECT policy, status, paused FROM agents WHERE id = ?1",
            [e.agent.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((policy, status, paused)) = agent else {
        return Ok(false);
    };
    if paused
        || !matches!(
            status.as_str(),
            "starting" | "idle" | "working" | "blocked" | "restarting"
        )
    {
        return Ok(false);
    }
    let policy: terrarium_core::config::Policy = serde_json::from_str(&policy)
        .map_err(|_| ApiError::internal("invalid stored workspace authority policy"))?;
    Ok(policy.targets.contains(&target)
        || super::leases::standing(t, &target, &e.agent.to_string())?
            == super::leases::Standing::Active)
}

/// Must share the FULL immediate intent transaction with operation allocation.
/// A revoke or target destruction can clear the current slot, never the seal.
pub fn reserve(
    t: &Tx<'_>,
    e: &Enrollment,
    registration: &str,
    domain: &str,
    owning_sandbox: &str,
) -> ApiResult<()> {
    no_link(t)?;
    durable(t)?;
    if owning_sandbox.is_empty() || identity(t, e)? != "fresh" {
        return Err(ApiError::new(
            Code::Conflict,
            "unknown workspace history or sandbox incarnation",
        ));
    }
    let valid: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM live_workspace_targets
         WHERE registration_id = ?1 AND host_id = ?2 AND trust_domain_id = ?3 AND state = 'verified')",
        params![registration, e.host.to_string(), domain], |r| r.get(0),
    )?;
    if !valid || !current_authority(t, e, registration)? {
        return Err(ApiError::new(
            Code::Conflict,
            "registration is not verified for this host and domain",
        ));
    }
    let existing: Option<(String, Option<String>, Option<String>)> = t
        .query_row(
            "SELECT trust_domain_id, current_registration_id, owning_sandbox_incarnation
         FROM workspace_reservations WHERE workspace_id = ?1",
            [e.workspace.0.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match existing {
        Some((sealed, current, sandbox)) => {
            if sealed != domain
                || current.as_deref().is_some_and(|r| r != registration)
                || sandbox.as_deref().is_some_and(|s| s != owning_sandbox)
            {
                return Err(ApiError::new(
                    Code::Conflict,
                    "workspace has a permanent domain seal or occupied registration",
                ));
            }
            t.execute(
                "UPDATE workspace_reservations SET current_registration_id = ?2,
                owning_sandbox_incarnation = ?3 WHERE workspace_id = ?1",
                params![e.workspace.0.to_string(), registration, owning_sandbox],
            )?;
        }
        None => {
            t.execute(
                "INSERT INTO workspace_reservations VALUES (?1, ?2, ?3, ?4)",
                params![
                    e.workspace.0.to_string(),
                    domain,
                    registration,
                    owning_sandbox
                ],
            )?;
        }
    }
    t.audit(
        "workspace.reserved",
        &e.workspace.0.to_string(),
        Some(&e.project),
        json!({"registration": registration, "domain": domain, "owning_sandbox": owning_sandbox}),
    )
}

/// Central check for an enrolled workspace. The caller must retain its
/// admission serialization through the side effect, not perform a read/check
/// and later reserve independently. Unknown identities fail closed.
pub fn admit_workspace_use(
    t: &Tx<'_>,
    e: &Enrollment,
    use_kind: WorkspaceUse<'_>,
) -> ApiResult<()> {
    identity(t, e)?;
    let reservation: Option<(String, Option<String>, Option<String>)> = t
        .query_row(
            "SELECT trust_domain_id, current_registration_id, owning_sandbox_incarnation
         FROM workspace_reservations WHERE workspace_id = ?1",
            [e.workspace.0.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((sealed, current, sandbox)) = reservation else {
        return match use_kind {
            WorkspaceUse::Other => Ok(()),
            _ => Err(ApiError::new(
                Code::Conflict,
                "workspace has no reservation",
            )),
        };
    };
    no_link(t)?;
    match use_kind {
        WorkspaceUse::OwnerSandbox { incarnation } if sandbox.as_deref() == Some(incarnation) => {
            Ok(())
        }
        WorkspaceUse::Live {
            registration,
            domain,
        } if sealed == domain && current.as_deref() == Some(registration) => {
            no_link(t)?;
            let verified: bool = t.query_row(
                    "SELECT EXISTS(SELECT 1 FROM live_workspace_targets r
                     JOIN live_workspace_attachments a ON a.registration_id = r.registration_id
                     WHERE r.registration_id = ?1 AND r.state = 'verified'
                     AND r.allocated_revision = r.last_contiguous_revision
                     AND a.workspace_id = ?2 AND a.agent_id = ?3 AND a.state = 'attached_verified'
                     AND a.revoke_revision IS NULL
                     AND NOT EXISTS(SELECT 1 FROM live_workspace_operations o
                         WHERE o.registration_id = r.registration_id AND o.phase != 'daemon_committed'))",
                    params![registration, e.workspace.0.to_string(), e.agent.to_string()], |r| r.get(0),
                )?;
            if verified && current_authority(t, e, registration)? {
                Ok(())
            } else {
                Err(ApiError::new(
                    Code::Conflict,
                    "workspace attachment is pending, revoked or quarantined",
                ))
            }
        }
        _ => Err(ApiError::new(
            Code::Conflict,
            "workspace use conflicts with its permanent reservation",
        )),
    }
}

/// Restart cannot infer attachment success from saved Docker inspect data.
/// Revoked tombstones and permanent seals are intentionally untouched.
pub fn quarantine_on_restart(t: &Tx<'_>) -> ApiResult<usize> {
    durable(t)?;
    let changed = t.execute(
        "UPDATE live_workspace_targets SET state = 'quarantined'
        WHERE state NOT IN ('quarantined', 'destroyed')",
        [],
    )?;
    t.execute(
        "UPDATE live_workspace_attachments SET state = 'quarantined'
        WHERE state NOT IN ('quarantined', 'path_revoked_verified')",
        [],
    )?;
    t.execute(
        "UPDATE live_workspace_operations SET phase = 'quarantined'
        WHERE phase NOT IN ('quarantined', 'daemon_committed')",
        [],
    )?;
    if changed > 0 {
        t.audit(
            "workspace.restart_quarantine",
            "live-workspaces",
            None,
            json!({"registrations": changed}),
        )?;
    }
    Ok(changed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{db::Db, events::EventHub};
    use terrarium_core::live_workspace::{SourceIdentity, WorkspaceId};
    use ulid::Ulid;

    async fn fixture() -> (tempfile::TempDir, Db, Enrollment) {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::db::open(&dir.path().join("test.sqlite")).unwrap();
        c.pragma_update(None, "synchronous", "NORMAL").unwrap();
        let db = Db::new(c, EventHub::default());
        let e = Enrollment {
            workspace: WorkspaceId(Ulid::new()),
            host: Ulid::new(),
            agent: terrarium_core::AgentId::new(),
            project: "fixture".into(),
            name: "holder".into(),
            source: SourceIdentity {
                device: 1,
                inode: 2,
            },
        };
        let enrollment = e.clone();
        db.write("system", move |t| {
            t.execute("INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at, status, policy, source)
                VALUES (?1, 'holder', 'fixture', 'codex', '/synthetic', 'now', 'now', 'idle', ?2, 'api')",
                params![enrollment.agent.to_string(), r#"{"targets":["target1","target2"]}"#])?;
            t.execute("INSERT INTO live_workspaces VALUES (?1, ?2, ?3, 'fixture', 'holder', '1', '2', 'fresh')",
                params![enrollment.workspace.0.to_string(), enrollment.host.to_string(), enrollment.agent.to_string()])?;
            for (registration, domain, target) in [("r1", "d1", "target1"), ("r2", "d2", "target2")] {
                t.execute("INSERT INTO live_workspace_targets (registration_id,host_id,target_id,trust_domain_id,
                    runtime_id,container_id,namespace_dev,namespace_ino) VALUES (?1,?2,?3,?4,'runtime',?5,'1','2')",
                    params![registration, enrollment.host.to_string(), target, domain, "a".repeat(64)])?;
                t.execute("UPDATE live_workspace_targets SET state = 'reconcile_pending' WHERE registration_id = ?1", [registration])?;
                t.execute("UPDATE live_workspace_targets SET state = 'verified' WHERE registration_id = ?1", [registration])?;
            }
            Ok(())
        }).await.unwrap();
        (dir, db, e)
    }

    #[tokio::test]
    async fn reservation_requires_full_durability_and_real_identity() {
        let (_dir, db, e) = fixture().await;
        let normal = e.clone();
        assert!(
            db.write("system", move |t| reserve(
                t, &normal, "r1", "d1", "sandbox"
            ))
            .await
            .is_err()
        );
        let mismatch = e.clone();
        assert!(
            db.write_full("system", move |t| {
                let mut mismatch = mismatch;
                mismatch.source.inode += 1;
                reserve(t, &mismatch, "r1", "d1", "sandbox")
            })
            .await
            .is_err()
        );
        db.write_full("system", move |t| reserve(t, &e, "r1", "d1", "sandbox"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn sealed_workspace_denies_generic_paths_and_wrong_sandbox() {
        let (_dir, db, e) = fixture().await;
        db.write_full("system", move |t| {
            reserve(t, &e, "r1", "d1", "sandbox")?;
            assert!(admit_workspace_use(t, &e, WorkspaceUse::Other).is_err());
            assert!(
                admit_workspace_use(
                    t,
                    &e,
                    WorkspaceUse::OwnerSandbox {
                        incarnation: "other"
                    }
                )
                .is_err()
            );
            admit_workspace_use(
                t,
                &e,
                WorkspaceUse::OwnerSandbox {
                    incarnation: "sandbox",
                },
            )
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn destruction_and_revoke_do_not_release_domain_seal() {
        let (_dir, db, e) = fixture().await;
        db.write_full("system", move |t| {
            reserve(t, &e, "r1", "d1", "sandbox")?;
            t.execute("UPDATE workspace_reservations SET current_registration_id = NULL WHERE workspace_id = ?1", [e.workspace.0.to_string()])?;
            t.execute("UPDATE live_workspace_targets SET state = 'destroyed' WHERE registration_id = 'r1'", [])?;
            assert!(reserve(t, &e, "r2", "d2", "sandbox").is_err());
            assert!(admit_workspace_use(t, &e, WorkspaceUse::Other).is_err());
            let domain: String = t.query_row("SELECT trust_domain_id FROM workspace_reservations", [], |r| r.get(0))?;
            assert_eq!(domain, "d1");
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_history_and_missing_sandbox_refuse_reservation() {
        let (_dir, db, e) = fixture().await;
        db.write_full("system", move |t| {
            assert!(reserve(t, &e, "r1", "d1", "").is_err());
            // The immutable enrollment cannot be rewritten to change history.
            assert!(
                t.execute("UPDATE live_workspaces SET history = 'unknown'", [])
                    .is_err()
            );
            let mut unknown = e.clone();
            unknown.workspace = WorkspaceId(Ulid::new());
            assert!(reserve(t, &unknown, "r1", "d1", "sandbox").is_err());
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn link_route_exception_never_authorizes_reservation() {
        let (_dir, db, e) = fixture().await;
        let result = crate::links::OPERATOR_ROUTE
            .scope(
                true,
                db.write_full("link:fixture", move |t| {
                    reserve(t, &e, "r1", "d1", "sandbox")
                }),
            )
            .await;
        assert!(matches!(
            result,
            Err(ApiError {
                code: Code::LinkForbidden,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn exec_requires_committed_attachment_current_authority_and_no_gap() {
        let (_dir, db, e) = fixture().await;
        db.write_full("system", move |t| {
            reserve(t, &e, "r1", "d1", "sandbox")?;
            let check = |t: &Tx<'_>| admit_workspace_use(t, &e, WorkspaceUse::Live { registration: "r1", domain: "d1" });
            assert!(check(t).is_err());
            t.execute("INSERT INTO live_workspace_attachments VALUES ('attachment', ?1, 'r1', ?2, 'attached_verified', NULL)",
                params![e.workspace.0.to_string(), e.agent.to_string()])?;
            check(t)?;
            t.execute("UPDATE agents SET status = 'stopped' WHERE id = ?1", [e.agent.to_string()])?;
            assert!(check(t).is_err());
            t.execute("UPDATE agents SET status = 'idle' WHERE id = ?1", [e.agent.to_string()])?;
            check(t)?;
            t.execute("UPDATE agents SET policy = '{}' WHERE id = ?1", [e.agent.to_string()])?;
            assert!(check(t).is_err());
            t.execute("UPDATE agents SET policy = ?2 WHERE id = ?1", params![e.agent.to_string(), r#"{"targets":["target1"]}"#])?;
            check(t)?;
            t.execute("UPDATE live_workspace_targets SET allocated_revision = 1 WHERE registration_id = 'r1'", [])?;
            assert!(check(t).is_err());
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn restart_quarantines_without_erasing_seal() {
        let (_dir, db, e) = fixture().await;
        db.write_full("system", move |t| {
            reserve(t, &e, "r1", "d1", "sandbox")?;
            assert_eq!(quarantine_on_restart(t)?, 2);
            assert!(
                admit_workspace_use(
                    t,
                    &e,
                    WorkspaceUse::Live {
                        registration: "r1",
                        domain: "d1"
                    }
                )
                .is_err()
            );
            assert!(admit_workspace_use(t, &e, WorkspaceUse::Other).is_err());
            let count: u32 =
                t.query_row("SELECT count(*) FROM workspace_reservations", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(count, 1);
            Ok(())
        })
        .await
        .unwrap();
    }
}
