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
    options.capabilities = HostCapabilities {
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
    use terrarium_core::{config::TargetKind, resources::Amounts};
    use terrariumd::store::{
        reservations::{self as r, Holder, Request},
        targets,
    };
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .write("system", |tx| {
            let r = r::request(
                tx,
                Request {
                    host: "h".into(),
                    project: "p".into(),
                    holder: Holder::Target("p-box".into()),
                    target: "p-box".into(),
                    amounts: Amounts {
                        cpu_millis: 1000,
                        ..Default::default()
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
