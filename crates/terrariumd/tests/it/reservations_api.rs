#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::json;
use std::time::Duration;
use terrarium_core::{
    limits::LimitsCapability,
    protocol::{HostCapabilities, ToolCapability},
    resources::HostCapacity,
};
use terrarium_host::{Client, ClientOptions};

#[tokio::test]
async fn old_reconciliation_cannot_open_a_new_connection_gate() {
    let td = TestDaemon::start().await;
    let executor = &td.daemon.as_ref().unwrap().state.executor;
    executor.set_resource_pending("h", true);
    let old = executor.resource_epoch("h");
    executor.set_resource_pending("h", true);
    assert!(!executor.finish_resource_reconcile("h", old.as_deref()));
    assert!(executor.resource_pending("h"));
    let current = executor.resource_epoch("h");
    assert!(executor.finish_resource_reconcile("h", current.as_deref()));
    assert!(!executor.resource_pending("h"));
}

#[tokio::test]
async fn internal_commands_participate_in_reservation_cleanup() {
    let (td, client, server) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let mut def = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-box")?.def))
        .await
        .unwrap();
    def.kind = terrarium_core::config::TargetKind::Host;
    let cmd = terrariumd::executor::RemoteCmd {
        argv: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        ..Default::default()
    };
    let running = terrariumd::executor::start_cmd(&state, &def, &cmd).unwrap();
    let names = vec![def.name.clone()];
    assert_eq!(state.executor.resource_sessions(&names, None).len(), 1);
    let collected = tokio::spawn(running.collect(1024));
    state.executor.end_resource_sessions(&names, None).await;
    let result = tokio::time::timeout(Duration::from_secs(5), collected)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result.end, Some(terrariumd::executor::End::Error { code, .. }) if code == "reservation_ended")
    );
    assert!(state.executor.resource_sessions(&names, None).is_empty());
    client.abort();
    server.abort();
}

async fn setup() -> (
    TestDaemon,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    setup_docker(false).await
}

async fn setup_docker(
    live: bool,
) -> (
    TestDaemon,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.agent("p", "lead", json!({"roles":["project_admin"]}))
        .await;
    td.agent("p", "worker", json!({"policy":{"targets":["p-box"]}}))
        .await;
    td.project("other").await;
    td.agent("other", "worker", json!({})).await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (server, addr) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0")
        .await
        .unwrap();
    let registered = td.post("/api/hosts", json!({"name":"h"})).await;
    let token = registered.ok()["token"].as_str().unwrap();
    let mut options = ClientOptions::new(format!("ws://{addr}/hosts/connect"), token, "h");
    use std::os::unix::fs::PermissionsExt;
    let docker = td.dir.path().join("reservation-docker");
    std::fs::write(&docker, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/docker.calls'\n[ ! -f '{}/docker.fail' ] || exit 1\n[ \"$1\" = update ] || exit 2\n", td.dir.path().display(), td.dir.path().display())).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
    options.docker_bin = docker.to_string_lossy().into_owned();
    let nvidia = td.dir.path().join("reservation-nvidia-smi");
    std::fs::write(
        &nvidia,
        "#!/bin/sh\nprintf 'GPU-abcd, test, 8192, 0\\nGPU-1234, test, 8192, 0\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&nvidia, std::fs::Permissions::from_mode(0o700)).unwrap();
    options.nvidia_smi_bin = nvidia.to_string_lossy().into_owned();
    options.capabilities = HostCapabilities {
        gpu_reservations: true,
        gpus: ["GPU-abcd", "GPU-1234"]
            .into_iter()
            .map(|uuid| terrarium_core::resources::GpuDevice {
                uuid: uuid.into(),
                name: "test".into(),
                memory_bytes: 8 << 30,
                used_bytes: 0,
            })
            .collect(),
        reservations: true,
        capacity: Some(HostCapacity {
            cpu_ids: vec![0, 1, 2, 3],
            memory_bytes: 8 << 30,
        }),
        docker: ToolCapability {
            present: true,
            version: Some("fake".into()),
        },
        limits: Some(LimitsCapability {
            docker: true,
            systemd: true,
            controllers: vec!["cpu".into(), "memory".into(), "io".into(), "cpuset".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    if live {
        options.docker_bin = "docker".into();
        options.capabilities.capacity =
            terrarium_host::capabilities::detect_capacity(std::path::Path::new("/proc"));
    }
    let client = tokio::spawn(Client::new(options).run());
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.executor.hosts.is_online("h") || state.executor.resource_pending("h") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for name in ["p-box", "p-box2"] {
        td.post(
            "/api/targets",
            json!({"name":name,"kind":"docker","host":"h","project":"p","image":if live { "alpine:3.22" } else { "fake" }}),
        )
        .await
        .ok();
    }
    td.put("/api/hosts/h/capacity", json!({"percentage":50}))
        .await
        .ok();
    (td, server, client)
}

#[tokio::test]
async fn exclusive_gpu_refuses_running_holder_and_unknown_container_exposure() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    td.patch("/api/targets/p-box", json!({"ephemeral":true}))
        .await
        .ok();
    state
        .db
        .write("system", |tx| {
            terrariumd::store::targets::set_state(tx, "p-box", "running", None)
        })
        .await
        .unwrap();
    let body =
        json!({"target":"p-box","gpus":[{"uuid":"GPU-abcd","exclusive":true}],"until":"30m"});
    let refused = td.post("/api/hosts/h/reservations", body.clone()).await;
    refused.err(409, "conflict");
    assert!(refused.body.to_string().contains("stop the target"));
    state
        .db
        .write("system", |tx| {
            terrariumd::store::targets::set_state(tx, "p-box", "absent", None)?;
            terrariumd::store::targets::set_state(tx, "p-box2", "running", None)
        })
        .await
        .unwrap();
    let refused = td.post("/api/hosts/h/reservations", body.clone()).await;
    refused.err(409, "conflict");
    assert!(refused.body.to_string().contains("incompatible containers"));
    // An ephemeral container without a known GPU reservation also has no
    // durable device-exposure baseline. Do not assume it was created GPU-free.
    state
        .db
        .write("system", |tx| {
            terrariumd::store::targets::set_state(tx, "p-box2", "absent", None)
        })
        .await
        .unwrap();
    td.patch("/api/targets/p-box2", json!({"ephemeral":true}))
        .await
        .ok();
    state
        .db
        .write("system", |tx| {
            terrariumd::store::targets::set_state(tx, "p-box2", "running", None)
        })
        .await
        .unwrap();
    td.post("/api/hosts/h/reservations", body)
        .await
        .err(409, "conflict");
    let rows = td.get("/api/hosts/h/reservations").await;
    assert!(
        rows.ok()
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["state"] == "retracted")
    );
    client.abort();
    server.abort();
}

#[tokio::test]
async fn disjoint_exclusive_gpus_can_run_concurrently() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    for target in ["p-box", "p-box2"] {
        td.patch(&format!("/api/targets/{target}"), json!({"ephemeral":true}))
            .await
            .ok();
    }
    td.post(
        "/api/hosts/h/reservations",
        json!({"target":"p-box","gpus":[{"uuid":"GPU-abcd","exclusive":true}],"until":"30m"}),
    )
    .await
    .ok();
    state
        .db
        .write("system", |tx| {
            terrariumd::store::targets::set_state(tx, "p-box", "running", None)
        })
        .await
        .unwrap();
    td.post(
        "/api/hosts/h/reservations",
        json!({"target":"p-box2","gpus":[{"uuid":"GPU-1234","exclusive":true}],"until":"30m"}),
    )
    .await
    .ok();
    let limits = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-box2")?.def))
        .await
        .unwrap();
    assert_eq!(
        terrariumd::reservations::docker_budget(&state, &limits)
            .unwrap()
            .unwrap()
            .gpu_devices,
        vec!["GPU-1234"]
    );
    client.abort();
    server.abort();
}

#[tokio::test]
async fn native_shared_gpu_monitor_sums_holder_scopes_without_enforcement() {
    use terrariumd::store::{agents, reservations as r};
    let (td, server, client) = setup().await;
    td.post(
        "/api/targets",
        json!({"name":"p-native","kind":"host","host":"h","project":"p"}),
    )
    .await
    .ok();
    let state = td.daemon.as_ref().unwrap().state.clone();
    // Seed an acknowledged host scope without invoking the real user systemd
    // manager on this shared build target. The API queue path is covered above.
    let id = state
        .db
        .write("system", |tx| {
            let agent = agents::find(tx, "p/worker", None)?;
            let record = r::request(
                tx,
                r::Request {
                    gpus: vec![terrarium_core::resources::GpuClaim {
                        uuid: "GPU-abcd".into(),
                        vram_bytes: 1 << 20,
                        exclusive: false,
                    }],
                    host: "h".into(),
                    project: "p".into(),
                    holder: r::Holder::Agent(agent.agent.id.to_string()),
                    target: "p-native".into(),
                    amounts: Default::default(),
                    exclusive: false,
                    reason: "test".into(),
                    duration_ms: 1_800_000,
                    queue_ms: None,
                },
                chrono::Utc::now().timestamp_millis(),
            )?;
            r::activate(tx, &record.id)?;
            Ok(record.id)
        })
        .await
        .unwrap();
    let slice = terrarium_host::limits::slice_name("_reservation", "p/worker");
    state.executor.hosts.record_metrics(
        "h",
        terrarium_host::protocol::HostMetrics {
            gpu_usage: vec![
                terrarium_host::gpu::Usage {
                    uuid: "GPU-abcd".into(),
                    container: String::new(),
                    cgroup: format!("/user.slice/{slice}/one.scope"),
                    used_bytes: 800_000,
                },
                terrarium_host::gpu::Usage {
                    uuid: "GPU-abcd".into(),
                    container: String::new(),
                    cgroup: format!("/user.slice/{slice}/two.scope"),
                    used_bytes: 800_000,
                },
            ],
            ..Default::default()
        },
        std::time::Instant::now(),
    );
    terrariumd::reservations::sweep(&state).await.unwrap();
    let record = state.db.read(move |c| r::get(c, &id)).await.unwrap();
    assert!(record.gpu_warned);
    assert_eq!(record.state, r::State::Active);
    let count: i64 = state
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM messages WHERE subject LIKE '%gpu_overage'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 2);
    client.abort();
    server.abort();
}

#[tokio::test]
async fn gpu_reservations_enforce_docker_boundary_and_warn_without_killing() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    td.post(
        "/api/targets",
        json!({"name":"p-native","kind":"host","host":"h","project":"p"}),
    )
    .await
    .ok();
    let refused = td.post("/api/hosts/h/reservations", json!({"target":"p-native","gpus":[{"uuid":"GPU-abcd","exclusive":true}],"until":"30m"})).await;
    assert!(!refused.status.is_success());
    assert!(refused.body.to_string().contains("exclusive GPU needs"));
    let body = json!({"target":"p-box","amounts":{},"gpus":[{"uuid":"GPU-abcd","exclusive":true}],"until":"30m"});
    assert!(
        !td.post("/api/hosts/h/reservations", body.clone())
            .await
            .status
            .is_success()
    );
    td.patch("/api/targets/p-box", json!({"ephemeral":true}))
        .await
        .ok();
    let grant = td.post("/api/hosts/h/reservations", body).await;
    let id = grant.ok()["id"].as_str().unwrap();
    assert!(grant.body.get("gpu_enforcement").is_none());
    let cooperative = td.post("/api/hosts/h/reservations", json!({"target":"p-native","gpus":[{"uuid":"GPU-abcd","vram_bytes":1048576}],"until":"30m","queue":true})).await;
    assert_eq!(cooperative.ok()["state"], "queued");
    let native_id = cooperative.body["id"].as_str().unwrap();
    td.post(&format!("/api/reservations/{native_id}/cancel"), json!({}))
        .await
        .ok();
    let capacity = td.get("/api/hosts/h/capacity").await;
    assert_eq!(
        capacity.ok()["used"]["gpus"][0]["claim"]["uuid"],
        "GPU-abcd"
    );
    let mut def = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-box")?.def))
        .await
        .unwrap();
    assert_eq!(
        terrariumd::reservations::docker_budget(&state, &def)
            .unwrap()
            .unwrap()
            .gpu_devices,
        vec!["GPU-abcd"]
    );
    def.kind = terrarium_core::config::TargetKind::Host;
    assert!(terrariumd::reservations::exec_budget(&state, &def, Some("p/worker")).is_ok());
    for kind in [
        terrarium_core::config::TargetKind::Ssh,
        terrarium_core::config::TargetKind::Vmlab,
    ] {
        def.kind = kind;
        assert!(terrariumd::reservations::exec_budget(&state, &def, None).is_err());
    }
    def.kind = terrarium_core::config::TargetKind::Host;
    let before: i64 = state
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?))
        .await
        .unwrap();
    for _ in 0..3 {
        assert!(terrariumd::reservations::exec_budget(&state, &def, Some("p/worker")).is_ok());
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    let count: i64 = state
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, before, "native admission must not generate any mail");
    td.delete(&format!("/api/reservations/{id}")).await.ok();
    let shared = td.post("/api/hosts/h/reservations", json!({"target":"p-box","amounts":{},"gpus":[{"uuid":"GPU-abcd","vram_bytes":1048576}],"until":"30m"})).await;
    let sid = shared.ok()["id"].as_str().unwrap();
    state.executor.hosts.record_metrics(
        "h",
        terrarium_host::protocol::HostMetrics {
            gpu_usage: vec![terrarium_host::gpu::Usage {
                cgroup: String::new(),
                uuid: "GPU-abcd".into(),
                container: "terrarium-p-box".into(),
                used_bytes: 2 << 20,
            }],
            ..Default::default()
        },
        std::time::Instant::now(),
    );
    terrariumd::reservations::sweep(&state).await.unwrap();
    terrariumd::reservations::sweep(&state).await.unwrap();
    let count: i64 = state
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM messages WHERE subject LIKE '%gpu_overage'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        count, 2,
        "holder and project lead, deduplicated across sweeps"
    );
    let reservation_id = sid.to_owned();
    state
        .db
        .write("system", move |tx| {
            use terrariumd::store::reservations::{get, gpu_usage};
            let id = &reservation_id;
            let start = get(tx, id)?.gpu_last_warning.unwrap();
            // Oscillation must not rearm, even after an hour.
            for offset in [30_000, 60_000, 3_600_000] {
                gpu_usage(tx, id, false, start + offset)?;
                gpu_usage(tx, id, true, start + offset + 1)?;
                assert!(get(tx, id)?.gpu_warned);
                assert_eq!(get(tx, id)?.gpu_last_warning, Some(start));
            }
            // A full hold rearms, but cannot bypass the hourly limit.
            gpu_usage(tx, id, false, start + 3_700_000)?;
            gpu_usage(tx, id, false, start + 4_299_999)?;
            assert!(get(tx, id)?.gpu_warned);
            gpu_usage(tx, id, false, start + 4_300_000)?;
            assert!(!get(tx, id)?.gpu_warned);
            gpu_usage(tx, id, true, start + 4_300_001)?;
            let last = start + 4_300_001;
            gpu_usage(tx, id, false, last + 1)?;
            gpu_usage(tx, id, false, last + 600_001)?;
            gpu_usage(tx, id, true, last + 600_002)?;
            assert_eq!(get(tx, id)?.gpu_last_warning, Some(last));
            gpu_usage(tx, id, true, last + 3_599_999)?;
            assert_eq!(get(tx, id)?.gpu_last_warning, Some(last));
            gpu_usage(tx, id, true, last + 3_600_000)?;
            assert_eq!(get(tx, id)?.gpu_last_warning, Some(last + 3_600_000));
            let mails: i64 = tx.query_row(
                "SELECT count(*) FROM messages WHERE subject=?1",
                [format!("Reservation {id}: gpu_overage")],
                |r| r.get(0),
            )?;
            assert_eq!(mails, 6, "three warnings to holder and lead");
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        td.get(&format!("/api/reservations/{sid}")).await.ok()["state"],
        "active"
    );
    assert!(
        !td.dir.path().join("docker.calls").exists(),
        "overage must not stop or update the container"
    );
    client.abort();
    server.abort();
}

#[tokio::test]
async fn host_reservation_refusal_names_existing_agent_sessions() {
    let (td, server, client) = setup().await;
    td.post(
        "/api/targets",
        json!({"name":"p-host","kind":"host","host":"h","project":"p"}),
    )
    .await
    .ok();
    let state = td.daemon.as_ref().unwrap().state.clone();
    let def = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-host")?.def))
        .await
        .unwrap();
    let cmd = terrariumd::executor::RemoteCmd {
        argv: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        holder: Some("p/worker".into()),
        ..Default::default()
    };
    let run = terrariumd::executor::start_cmd(&state, &def, &cmd).unwrap();
    let names = vec!["p-host".to_owned()];
    let sessions = state.executor.resource_sessions(&names, None);
    assert_eq!(sessions.len(), 1);
    for agent in [Some("p/worker"), None] {
        let mut body = json!({"target":"p-host","amounts":{"cpu_millis":1000},"until":"30m"});
        if let Some(agent) = agent {
            body["agent"] = json!(agent);
        }
        let refused = td.post("/api/hosts/h/reservations", body).await;
        refused.err(409, "conflict");
        assert_eq!(
            refused.body["error"]["details"]["sessions"],
            json!(sessions),
            "{}",
            refused.body
        );
    }
    state.executor.end_resource_sessions(&names, None).await;
    run.collect(1024).await;
    client.abort();
    server.abort();
}

#[tokio::test]
async fn slow_docker_pull_does_not_hold_admission_or_block_another_host() {
    use terrarium_core::protocol::LifecycleVerb;
    use terrariumd::executor::{LifecycleOpts, lifecycle};
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let directory = td.dir.path();
    std::fs::write(directory.join("reservation-docker"), format!(
        "#!/bin/sh\ncase $1 in\ninspect) echo '[]'; echo 'Error: No such object' >&2; exit 1;;\nrun) touch '{0}/pull-entered'; while [ ! -f '{0}/pull-release' ]; do sleep 0.05; done;;\nupdate) exit 0;;\nesac\n", directory.display()
    )).unwrap();
    let (server_b, address) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0")
        .await
        .unwrap();
    let registration = td.post("/api/hosts", json!({"name":"other-host"})).await;
    let mut options = ClientOptions::new(
        format!("ws://{address}/hosts/connect"),
        registration.ok()["token"].as_str().unwrap(),
        "other-host",
    );
    options.docker_bin = directory
        .join("reservation-docker")
        .to_string_lossy()
        .into_owned();
    options.capabilities = state.executor.hosts.capabilities("h").unwrap();
    let client_b = tokio::spawn(Client::new(options).run());
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.executor.hosts.is_online("other-host")
            || state.executor.resource_pending("other-host")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    td.post(
        "/api/targets",
        json!({"name":"p-fast","kind":"docker","host":"other-host","project":"p","image":"fake"}),
    )
    .await
    .ok();
    let (slow, fast) = state
        .db
        .read(|c| {
            Ok((
                terrariumd::store::targets::get(c, "p-box")?.def,
                terrariumd::store::targets::get(c, "p-fast")?.def,
            ))
        })
        .await
        .unwrap();
    let task_state = state.clone();
    let pull = tokio::spawn(async move {
        lifecycle(
            &task_state,
            &slow,
            LifecycleVerb::Up,
            &LifecycleOpts::default(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !directory.join("pull-entered").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(state.executor.resource_lock("h").try_lock_owned().is_ok());
    // The pull cannot complete until we release its file gate below. Allow
    // shared CI scheduling delays without weakening the independence check.
    tokio::time::timeout(Duration::from_secs(15), async {
        lifecycle(
            &state,
            &fast,
            LifecycleVerb::Status,
            &LifecycleOpts::default(),
        )
        .await
        .unwrap();
        td.put("/api/hosts/other-host/capacity", json!({"percentage":50}))
            .await
            .ok();
        td.post(
            "/api/hosts/other-host/reservations",
            json!({"target":"p-fast","amounts":{"cpu_millis":1000},"until":"30m"}),
        )
        .await
        .ok();
    })
    .await
    .unwrap();
    td.put("/api/hosts/h/capacity", json!({"percentage":50}))
        .await
        .err(409, "resource_busy");
    let same_host = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-box2")?.def))
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(15),
        lifecycle(
            &state,
            &same_host,
            LifecycleVerb::Status,
            &LifecycleOpts::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    // The completed Status must not clear the still-running Up marker.
    assert!(state.executor.resource_verb_idle("h").is_err());
    td.post(
        "/api/hosts/h/reservations",
        json!({
            "target":"p-box2", "amounts":{"cpu_millis":1000}, "until":"30m"
        }),
    )
    .await
    .err(409, "resource_busy");
    let queued = td
        .post(
            "/api/hosts/h/reservations",
            json!({
                "target":"p-box2", "amounts":{"cpu_millis":1000}, "until":"30m", "queue":true
            }),
        )
        .await;
    assert_eq!(queued.ok()["state"], "queued");
    td.patch("/api/targets/p-box", json!({"keep":true}))
        .await
        .ok();
    td.patch("/api/targets/p-box", json!({"limits":{"cpu":1}}))
        .await
        .err(409, "resource_busy");
    std::fs::write(directory.join("pull-release"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(5), pull)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(state.executor.resource_verb_idle("h").is_ok());
    terrariumd::reservations::sweep(&state).await.unwrap();
    let id = queued.body["id"].as_str().unwrap();
    assert_eq!(
        td.get(&format!("/api/reservations/{id}")).await.ok()["state"],
        "active"
    );
    client.abort();
    server.abort();
    client_b.abort();
    server_b.abort();
}

#[tokio::test]
async fn command_waits_briefly_for_admission_and_lock_keys_do_not_collide() {
    use terrariumd::executor::{RemoteCmd, start_cmd};
    let (td, server, client) = setup().await;
    td.post(
        "/api/targets",
        json!({"name":"p-host","kind":"host","host":"h","project":"p"}),
    )
    .await
    .ok();
    let state = td.daemon.as_ref().unwrap().state.clone();
    let def = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-host")?.def))
        .await
        .unwrap();
    let lock = state.executor.resource_lock("h");
    let guard = lock.lock_owned().await;
    let mut hostless = def.clone();
    hostless.host = None;
    hostless.name = "h".to_owned();
    assert!(
        state
            .executor
            .target_resource_lock(&hostless)
            .try_lock_owned()
            .is_ok()
    );
    let cmd = RemoteCmd {
        argv: vec!["true".into()],
        ..Default::default()
    };
    let run = start_cmd(&state, &def, &cmd).unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(guard);
    assert!(run.collect(1024).await.end.unwrap().success());
    let _guard = state.executor.resource_lock("h").lock_owned().await;
    let run = start_cmd(&state, &def, &cmd).unwrap();
    assert!(
        matches!(run.collect(1024).await.end.unwrap(), terrariumd::executor::End::Error { code, .. } if code == "resource_busy")
    );
    client.abort();
    server.abort();
}

#[tokio::test]
#[ignore = "requires an exclusive Docker scratch host, alpine:3.22 and TERRARIUM_LIVE_IO_DEVICE"]
async fn live_docker_release_and_create_only_io() {
    use std::process::Command;
    fn inspect(name: &str) -> serde_json::Value {
        let output = Command::new("docker")
            .args(["inspect", name])
            .output()
            .unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()[0].clone()
    }
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for name in ["terrarium-p-box", "terrarium-p-box2"] {
                let _ = Command::new("docker").args(["rm", "-f", name]).output();
            }
        }
    }
    let device =
        std::env::var("TERRARIUM_LIVE_IO_DEVICE").expect("explicit scratch block device required");
    for name in ["terrarium-p-box", "terrarium-p-box2"] {
        assert!(
            !Command::new("docker")
                .args(["inspect", name])
                .output()
                .unwrap()
                .status
                .success(),
            "scratch name already exists"
        );
    }
    let _cleanup = Cleanup;
    let (td, server, client) = setup_docker(true).await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let physical = state
        .executor
        .hosts
        .capabilities("h")
        .unwrap()
        .capacity
        .unwrap();
    td.put(
        "/api/hosts/h/capacity",
        json!({"percentage":100,"io":{device.clone():{"read_bps":1048576}}}),
    )
    .await
    .ok();
    td.post("/api/targets/p-box/up", json!({})).await.ok();
    let granted = td.post("/api/hosts/h/reservations", json!({"target":"p-box","amounts":{"cpu_millis":500,"memory_bytes":67108864},"until":"30m"})).await;
    let id = granted.ok()["id"].as_str().unwrap();
    let limited = inspect("terrarium-p-box");
    assert_eq!(limited["HostConfig"]["NanoCpus"], 500000000_u64);
    assert_eq!(limited["HostConfig"]["Memory"], 67108864_u64);
    td.delete(&format!("/api/reservations/{id}")).await.ok();
    let released = inspect("terrarium-p-box");
    assert_eq!(released["State"]["Running"], true);
    assert_eq!(
        released["HostConfig"]["NanoCpus"],
        physical.cpu_ids.len() as u64 * 1_000_000_000
    );
    assert_eq!(released["HostConfig"]["Memory"], physical.memory_bytes);
    assert_eq!(
        released["HostConfig"]["CpusetCpus"],
        physical
            .cpu_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("released: {}", released["HostConfig"]);
    td.post(
        "/api/hosts/h/reservations",
        json!({"target":"p-box","amounts":{"io":{device.clone():{"read_bps":1024}}},"until":"30m"}),
    )
    .await
    .err(409, "conflict");
    td.patch("/api/targets/p-box2", json!({"ephemeral":true}))
        .await
        .ok();
    let io = td.post("/api/hosts/h/reservations", json!({"target":"p-box2","amounts":{"io":{device.clone():{"read_bps":1048576}}},"until":"30m"})).await;
    let io_id = io.ok()["id"].as_str().unwrap();
    td.post("/api/targets/p-box2/up", json!({})).await.ok();
    assert_eq!(
        inspect("terrarium-p-box2")["HostConfig"]["BlkioDeviceReadBps"][0]["Rate"],
        1048576_u64
    );
    td.delete(&format!("/api/reservations/{io_id}")).await.ok();
    let output = Command::new("docker")
        .args(["inspect", "terrarium-p-box2"])
        .output()
        .unwrap();
    if output.status.success() {
        assert_ne!(inspect("terrarium-p-box2")["State"]["Running"], true);
    }
    client.abort();
    server.abort();
}

#[tokio::test]
async fn fixed_docker_limits_update_live_and_failed_ack_keeps_defaults() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .write("system", |tx| {
            let row = terrariumd::store::targets::get(tx, "p-box")?;
            let mut runtime = row.runtime;
            runtime.docker_settings =
                Some(terrariumd::store::targets::DockerSettings::of(&row.def));
            terrariumd::store::targets::set_runtime(tx, "p-box", &runtime)?;
            terrariumd::store::targets::set_state(tx, "p-box", "running", None)
        })
        .await
        .unwrap();
    td.patch(
        "/api/targets/p-box",
        json!({"limits":{"cpu":1,"memory":"64M"}}),
    )
    .await
    .ok();
    let calls = std::fs::read_to_string(td.dir.path().join("docker.calls")).unwrap();
    assert!(calls.contains("update"));
    assert!(calls.contains("--cpus=1"));
    let acknowledged = state
        .db
        .read(|c| terrariumd::store::targets::get(c, "p-box"))
        .await
        .unwrap();
    assert!(!acknowledged.runtime.recreate_required);
    assert_eq!(
        acknowledged.runtime.docker_settings,
        Some(terrariumd::store::targets::DockerSettings::of(
            &acknowledged.def
        ))
    );
    std::fs::write(td.dir.path().join("docker.fail"), "fail").unwrap();
    let refused = td
        .patch("/api/targets/p-box", json!({"limits":{"cpu":2}}))
        .await;
    assert!(!refused.status.is_success());
    let saved = state
        .db
        .read(|c| terrariumd::store::targets::get(c, "p-box"))
        .await
        .unwrap();
    assert_eq!(
        saved.def.limits.unwrap().resolve().unwrap().cpu_millis,
        Some(1000)
    );
    assert_eq!(saved.runtime, acknowledged.runtime);
    client.abort();
    server.abort();
}

#[tokio::test]
async fn combined_image_and_limits_edit_keeps_recreate_required() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .write("system", |tx| {
            let row = terrariumd::store::targets::get(tx, "p-box")?;
            let mut runtime = row.runtime;
            runtime.docker_settings =
                Some(terrariumd::store::targets::DockerSettings::of(&row.def));
            terrariumd::store::targets::set_runtime(tx, "p-box", &runtime)?;
            terrariumd::store::targets::set_state(tx, "p-box", "stopped", None)
        })
        .await
        .unwrap();
    td.patch(
        "/api/targets/p-box",
        json!({"image":"new-image","limits":{"cpu":1}}),
    )
    .await
    .ok();
    let row = state
        .db
        .read(|c| terrariumd::store::targets::get(c, "p-box"))
        .await
        .unwrap();
    assert!(row.runtime.recreate_required);
    assert_eq!(row.def.image.as_deref(), Some("new-image"));
    assert!(
        std::fs::read_to_string(td.dir.path().join("docker.calls"))
            .unwrap()
            .contains("--cpus=1")
    );
    client.abort();
    server.abort();
}

#[tokio::test]
async fn missing_host_ack_at_api_keeps_limits_and_baseline() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .write("system", |tx| {
            let row = terrariumd::store::targets::get(tx, "p-box")?;
            let mut runtime = row.runtime;
            runtime.docker_settings =
                Some(terrariumd::store::targets::DockerSettings::of(&row.def));
            terrariumd::store::targets::set_runtime(tx, "p-box", &runtime)?;
            terrariumd::store::targets::set_state(tx, "p-box", "stopped", None)
        })
        .await
        .unwrap();
    let before = state
        .db
        .read(|c| terrariumd::store::targets::get(c, "p-box"))
        .await
        .unwrap();
    client.abort();
    let _ = client.await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.executor.hosts.is_online("h") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !td.patch("/api/targets/p-box", json!({"limits":{"cpu":2}}))
            .await
            .status
            .is_success()
    );
    let after = state
        .db
        .read(|c| terrariumd::store::targets::get(c, "p-box"))
        .await
        .unwrap();
    assert_eq!(after.def.limits, before.def.limits);
    assert_eq!(
        after.runtime.docker_settings,
        before.runtime.docker_settings
    );
    server.abort();
}

#[tokio::test]
async fn contended_command_errors_are_end_events_and_dropped_controls_kill() {
    use terrariumd::executor::{End, RemoteCmd, start_cmd};
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let mut def = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-box")?.def))
        .await
        .unwrap();
    def.kind = terrarium_core::config::TargetKind::Host;
    let guard = state.executor.resource_lock("h").lock_owned().await;
    let mut bad = def.clone();
    bad.host = Some("missing-host".into());
    let other_guard = state
        .executor
        .resource_lock("missing-host")
        .lock_owned()
        .await;
    let run = start_cmd(
        &state,
        &bad,
        &RemoteCmd {
            argv: vec!["true".into()],
            ..Default::default()
        },
    )
    .unwrap();
    drop(other_guard);
    assert!(matches!(
        run.collect(1024).await.end,
        Some(End::Error { .. })
    ));
    let run = start_cmd(
        &state,
        &def,
        &RemoteCmd {
            argv: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            ..Default::default()
        },
    )
    .unwrap();
    drop(guard);
    let terrariumd::executor::Running { ctl, mut events } = run;
    while let Some(event) = events.recv().await {
        if matches!(event, terrariumd::executor::Ev::Started) {
            break;
        }
    }
    drop(ctl);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if matches!(event, terrariumd::executor::Ev::End(_)) {
                return;
            }
        }
    })
    .await
    .unwrap();
    client.abort();
    server.abort();
}

#[tokio::test]
async fn unacknowledged_limits_do_not_advance_the_container_baseline() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (baseline, updated) = state
        .db
        .write("system", |tx| {
            let row = terrariumd::store::targets::get(tx, "p-box")?;
            let baseline = terrariumd::store::targets::DockerSettings::of(&row.def);
            let mut runtime = row.runtime;
            runtime.docker_settings = Some(baseline.clone());
            terrariumd::store::targets::set_runtime(tx, "p-box", &runtime)?;
            let updated = terrariumd::store::targets::update_with_live_limits(
                tx,
                "p-box",
                &json!({"limits":{"cpu":1}}),
                true,
                false,
            )?;
            Ok((baseline, updated))
        })
        .await
        .unwrap();
    assert_eq!(updated.runtime.docker_settings, Some(baseline));
    assert!(updated.runtime.recreate_required);
    client.abort();
    server.abort();
}

#[tokio::test]
async fn active_docker_budget_overrides_edits_and_retract_restores_defaults() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .write("system", |tx| {
            terrariumd::store::targets::set_state(tx, "p-box", "running", None)
        })
        .await
        .unwrap();
    let granted = td
        .post(
            "/api/hosts/h/reservations",
            json!({
                "target":"p-box","amounts":{"cpu_millis":2000},"until":"30m"
            }),
        )
        .await;
    let id = granted.ok()["id"].as_str().unwrap();
    td.patch("/api/targets/p-box", json!({"limits":{"cpu":1}}))
        .await
        .ok();
    let calls = std::fs::read_to_string(td.dir.path().join("docker.calls")).unwrap();
    assert!(calls.lines().last().unwrap().contains("--cpus=2"));
    td.delete(&format!("/api/reservations/{id}")).await.ok();
    let calls = std::fs::read_to_string(td.dir.path().join("docker.calls")).unwrap();
    assert!(calls.lines().last().unwrap().contains("--cpus=1"));
    assert!(
        calls
            .lines()
            .all(|line| !line.starts_with("stop ") && !line.starts_with("rm "))
    );
    let saved = state
        .db
        .read(|c| terrariumd::store::targets::get(c, "p-box"))
        .await
        .unwrap();
    assert_eq!(saved.state, "running");
    td.patch("/api/targets/p-box", json!({"limits":null}))
        .await
        .ok();
    let calls = std::fs::read_to_string(td.dir.path().join("docker.calls")).unwrap();
    let released = calls
        .lines()
        .rfind(|line| line.starts_with("update "))
        .unwrap();
    assert!(released.contains("--cpus=4"));
    assert!(released.contains("--memory=8589934592"));
    assert!(released.contains("--cpuset-cpus=0,1,2,3"));
    client.abort();
    server.abort();
}

#[tokio::test]
async fn reconnect_gate_blocks_exec_until_active_limits_are_restored() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .write("system", |tx| {
            terrariumd::store::targets::set_state(tx, "p-box", "running", None)
        })
        .await
        .unwrap();
    td.post(
        "/api/hosts/h/reservations",
        json!({"target":"p-box","amounts":{"cpu_millis":1000},"until":"30m"}),
    )
    .await
    .ok();
    let def = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-box")?.def))
        .await
        .unwrap();
    state.executor.set_resource_pending("h", true);
    assert!(terrariumd::reservations::exec_budget(&state, &def, None).is_err());
    std::fs::write(td.dir.path().join("docker.fail"), "fail").unwrap();
    assert!(
        terrariumd::reservations::reconcile_host(&state, "h")
            .await
            .is_err()
    );
    assert!(state.executor.resource_pending("h"));
    std::fs::remove_file(td.dir.path().join("docker.fail")).unwrap();
    terrariumd::reservations::reconcile_host(&state, "h")
        .await
        .unwrap();
    assert!(!state.executor.resource_pending("h"));
    assert!(terrariumd::reservations::exec_budget(&state, &def, None).is_ok());
    client.abort();
    server.abort();
}

#[tokio::test]
async fn permanent_docker_refuses_create_only_io_reservations() {
    let (td, server, client) = setup().await;
    td.put(
        "/api/hosts/h/capacity",
        json!({"percentage":50,"io":{"8:0":{"read_bps":1000000}}}),
    )
    .await
    .ok();
    let response = td
        .post(
            "/api/hosts/h/reservations",
            json!({
                "target":"p-box", "amounts":{"io":{"8:0":{"read_bps":1000}}}, "until":"30m"
            }),
        )
        .await;
    response.err(409, "conflict");
    assert!(
        response
            .body
            .to_string()
            .contains("cannot be cleared without recreating")
    );
    client.abort();
    server.abort();
}

#[tokio::test]
async fn capacity_configuration_and_grants_obey_lease_roles() {
    let (td, server, client) = setup().await;
    td.put_as("p/lead", "/api/hosts/h/capacity", json!({"percentage":100}))
        .await
        .err(403, "forbidden");
    let body = json!({"target":"p-box","amounts":{"cpu_millis":2000},"until":"30m"});
    td.post_as("p/worker", "/api/hosts/h/reservations", body.clone())
        .await
        .err(403, "forbidden");
    let grant = td
        .post_as("p/lead", "/api/hosts/h/reservations", body)
        .await;
    let id = grant.ok()["id"].as_str().unwrap();
    assert_eq!(grant.body["state"], "active");
    td.get_as("other/worker", &format!("/api/reservations/{id}"))
        .await
        .err(403, "forbidden");
    assert!(
        td.get_as("other/worker", "/api/hosts/h/reservations")
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );
    td.post_as(
        "p/lead",
        "/api/hosts/h/reservations",
        json!({"target":"p-box2","amounts":{"cpu_millis":1000},"until":"30m"}),
    )
    .await
    .err(409, "conflict");
    client.abort();
    server.abort();
}

#[tokio::test]
async fn queued_grant_cancels_and_holder_receives_warning_mail() {
    let (td, server, client) = setup().await;
    let grant = td
        .post(
            "/api/hosts/h/reservations",
            json!({"target":"p-box","amounts":{"cpu_millis":2000},"until":"4m"}),
        )
        .await;
    let id = grant.ok()["id"].as_str().unwrap().to_owned();
    let queued = td.post("/api/hosts/h/reservations", json!({"target":"p-box2","amounts":{"cpu_millis":1000},"until":"30m","queue":true,"queue_timeout":"10m"})).await;
    assert_eq!(queued.ok()["state"], "queued");
    let qid = queued.body["id"].as_str().unwrap();
    td.post_as(
        "other/worker",
        "/api/agents/other%2Fworker/waits",
        json!({"kind":"reservation","reservation":qid}),
    )
    .await
    .err(403, "forbidden");
    let wait = td
        .post_as(
            "p/worker",
            "/api/agents/p%2Fworker/waits",
            json!({"kind":"reservation","reservation":qid}),
        )
        .await;
    let wid = wait.ok()["id"].as_str().unwrap();
    assert_eq!(wait.body["state"], "open");
    td.post(&format!("/api/reservations/{qid}/cancel"), json!({}))
        .await
        .ok();
    assert_eq!(
        td.get(&format!("/api/waits/{wid}")).await.ok()["state"],
        "failed"
    );
    let state = td.daemon.as_ref().unwrap().state.clone();
    terrariumd::reservations::sweep(&state).await.unwrap();
    let (mail, pointers) = state
        .db
        .read(move |c| {
            let mail: i64 = c.query_row(
                "SELECT count(*) FROM messages WHERE subject=?1",
                [format!("Reservation {id}: warning")],
                |r| r.get(0),
            )?;
            let pointers: i64 = c.query_row(
                "SELECT count(*) FROM events WHERE kind='reservation_pointer'",
                [],
                |r| r.get(0),
            )?;
            Ok((mail, pointers))
        })
        .await
        .unwrap();
    assert_eq!(mail, 1);
    assert!(pointers >= 2);
    client.abort();
    server.abort();
}

#[tokio::test]
async fn reservation_wait_resolves_on_start_and_reconciles_already_active() {
    let (td, server, client) = setup().await;
    let first = td
        .post(
            "/api/hosts/h/reservations",
            json!({"target":"p-box","amounts":{"cpu_millis":2000},"until":"30m"}),
        )
        .await;
    let first_id = first.ok()["id"].as_str().unwrap();
    let queued = td
        .post(
            "/api/hosts/h/reservations",
            json!({"target":"p-box2","amounts":{"cpu_millis":1000},"until":"30m","queue":true}),
        )
        .await;
    let id = queued.ok()["id"].as_str().unwrap();
    let wait = td
        .post_as(
            "p/worker",
            "/api/agents/p%2Fworker/waits",
            json!({"kind":"reservation","reservation":id}),
        )
        .await;
    let wid = wait.ok()["id"].as_str().unwrap();
    td.delete(&format!("/api/reservations/{first_id}"))
        .await
        .ok();
    let state = td.daemon.as_ref().unwrap().state.clone();
    terrariumd::reservations::sweep(&state).await.unwrap();
    assert_eq!(
        td.get(&format!("/api/waits/{wid}")).await.ok()["state"],
        "resolved"
    );
    let late = td
        .post_as(
            "p/worker",
            "/api/agents/p%2Fworker/waits",
            json!({"kind":"reservation","reservation":id}),
        )
        .await;
    let late_id = late.ok()["id"].as_str().unwrap();
    state
        .db
        .write("system", terrariumd::store::waits::check_reservation_waits)
        .await
        .unwrap();
    assert_eq!(
        td.get(&format!("/api/waits/{late_id}")).await.ok()["state"],
        "resolved"
    );
    client.abort();
    server.abort();
}

#[tokio::test]
async fn exclusive_budget_pins_owner_and_excludes_shared_workloads() {
    use terrarium_core::{
        config::TargetKind,
        resources::{Amounts, IoAmounts},
    };
    use terrariumd::store::{
        reservations::{self as r, Holder, Request},
        targets,
    };
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .write("system", |tx| {
            let io = std::collections::BTreeMap::from([(
                "8:0".to_owned(),
                IoAmounts {
                    read_bps: 1000,
                    write_bps: 2000,
                    read_iops: 30,
                    write_iops: 40,
                },
            )]);
            r::configure(tx, "h", 100, io.clone())?;
            let r = r::request(
                tx,
                Request {
                    gpus: vec![],
                    host: "h".into(),
                    project: "p".into(),
                    holder: Holder::Target("p-box".into()),
                    target: "p-box".into(),
                    amounts: Amounts {
                        cpu_millis: 1000,
                        memory_bytes: 16 << 20,
                        io,
                    },
                    exclusive: true,
                    reason: "test".into(),
                    duration_ms: 1_800_000,
                    queue_ms: None,
                },
                chrono::Utc::now().timestamp_millis(),
            )?;
            r::activate(tx, &r.id)?;
            Ok(())
        })
        .await
        .unwrap();
    let capacity = td.get("/api/hosts/h/capacity").await;
    assert_eq!(capacity.ok()["used"]["exclusive_cores"], json!([0]));
    assert_eq!(
        capacity.body["used"]["io"]["8:0"],
        json!({"read_bps":1000,"write_bps":2000,"read_iops":30,"write_iops":40})
    );
    assert_eq!(capacity.body["used"]["cpu_millis"], 1000);
    assert_eq!(capacity.body["used"]["memory_bytes"], 16 << 20);
    let (owner, shared) = state
        .db
        .read(|c| {
            Ok((
                targets::get(c, "p-box")?.def,
                targets::get(c, "p-box2")?.def,
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        terrariumd::reservations::docker_budget(&state, &owner)
            .unwrap()
            .unwrap()
            .cpus,
        Some(vec![0])
    );
    assert_eq!(
        terrariumd::reservations::docker_budget(&state, &shared)
            .unwrap()
            .unwrap()
            .cpus,
        Some(vec![1, 2, 3])
    );
    let mut host = shared.clone();
    host.kind = TargetKind::Host;
    assert!(
        terrariumd::reservations::exec_budget(&state, &host, Some("p/worker"))
            .unwrap()
            .is_some()
    );
    for kind in [TargetKind::Vmlab, TargetKind::Ssh] {
        host.kind = kind;
        assert!(terrariumd::reservations::lifecycle_admission(&state, &host).is_err());
        assert!(terrariumd::reservations::exec_budget(&state, &host, Some("p/worker")).is_err());
    }
    host.kind = TargetKind::Docker;
    host.image = None;
    assert!(terrariumd::reservations::lifecycle_admission(&state, &host).is_err());
    assert!(terrariumd::reservations::exec_budget(&state, &host, Some("p/worker")).is_err());
    client.abort();
    server.abort();
}
