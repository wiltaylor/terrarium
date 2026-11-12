// Runtime registry entries are the boundary. These additional reviewed cases
// exercise conditional actions and real approval resources.
#[derive(serde::Deserialize)]
struct LinkRouteInventory {
    routes: Vec<LinkRouteEntry>,
}
#[derive(serde::Deserialize)]
struct LinkRouteEntry {
    cases: Vec<LinkRouteCase>,
}
#[derive(serde::Deserialize)]
struct LinkRouteCase {
    method: String,
    path: String,
    #[serde(default)]
    body: serde_json::Value,
    #[serde(default)]
    allowed: bool,
    #[serde(default)]
    approval: bool,
}
fn link_inventory() -> LinkRouteInventory {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/link-route-access.json"
    )))
    .unwrap()
}

async fn link_approval_fixtures(
    s: &Shared,
    dev: String,
) -> std::collections::BTreeMap<String, String> {
    s.db.write("operator", move |t| {
        use crate::store::{board, questions, spawns, waits};
        let tool = questions::ask_tool_run(
            t,
            &dev,
            "review",
            "target",
            &["inspect".into()],
            "2099-01-01T00:00:00Z",
        )?;
        t.execute(
            "UPDATE projects SET github_authors = '[\"trusted\"]' WHERE name = 'p'",
            [],
        )?;
        let actor = crate::actor::Actor::Agent(crate::actor::AgentActor {
            id: dev.clone(),
            name: "dev".into(),
            project: "p".into(),
            roles: vec![terrarium_core::Role::ProjectAdmin],
            policy: Default::default(),
        });
        let card = board::create_card(
            t,
            &actor,
            "p",
            board::NewCard {
                title: Some("GitHub #1: held".into()),
                ..Default::default()
            },
        )?;
        let hold: String = t.query_row(
            "SELECT id FROM questions WHERE hold_card = ?1",
            [&card.id],
            |r| r.get(0),
        )?;
        spawns::insert(
            t,
            &spawns::NewSpawn {
                id: &dev,
                owner: "operator",
                owner_name: "operator",
                project: "p",
                name: "dev",
                template: "worker",
                harness: "codex",
                task: "private task",
                deadline_at: "2099-01-01T00:00:00Z",
                bound: None,
            },
        )?;
        let watch = waits::create_watch(
            t,
            waits::NewWatch {
                watcher: "operator".into(),
                agent: dev.clone(),
                on: vec![terrarium_core::AgentStatus::Idle],
                once: false,
                debounce: true,
            },
        )?;
        Ok(std::collections::BTreeMap::from([
            ("$tool".into(), tool.id),
            ("$hold".into(), hold),
            ("$card".into(), card.id),
            ("$spawn".into(), dev.clone()),
            ("$dev".into(), dev),
            ("$watch".into(), watch.id),
        ]))
    })
    .await
    .unwrap()
}

async fn link_approval_snapshot(s: &Shared) -> serde_json::Value {
    s.db.read(|c| {
        // Include approvals, the held card, wakes, mail, events, and audit.
        // Failed link approvals must leave all of these unchanged.
        let mut result = serde_json::Map::new();
        for table in ["questions", "cards", "waits", "messages", "events", "audit"] {
            let mut stmt = c.prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
            let count = stmt.column_count();
            let rows = stmt
                .query_map([], |row| {
                    Ok((0..count)
                        .map(|i| format!("{:?}", row.get_ref(i)))
                        .collect::<Vec<_>>())
                })?
                .collect::<Result<Vec<_>, _>>()?;
            result.insert(table.into(), serde_json::json!(rows));
        }
        Ok(serde_json::Value::Object(result))
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn link_operator_routes_deny_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let s = state_at(dir.path());
    let (_, dev) = seed_sysadmin(&s).await;
    let (_, key) = paired(&s, false).await;
    let fixture = link_approval_fixtures(&s, dev).await;
    let auth = format!("Bearer {key}");
    let h = [("authorization", auth.as_str())];
    let (_, registry) = routes_with_registry(s.clone(), false, false);
    for entry in &registry.entries {
        if entry.access != link_registry::Access::OperatorOnly {
            continue;
        }
        let path = entry
            .path
            .split('/')
            .map(|segment| {
                if segment.starts_with('{') {
                    "link-test"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let response = send_body(
            link_app(&s),
            entry.method.as_str(),
            &path,
            &h,
            &serde_json::json!({}),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{} {path}",
            entry.method
        );
    }
    let mut approvals = Vec::new();
    for entry in link_inventory().routes {
        for case in entry.cases {
            let mut path = case.path.clone();
            for (key, value) in &fixture {
                path = path.replace(key, value);
            }
            let before = if case.approval {
                Some(link_approval_snapshot(&s).await)
            } else {
                None
            };
            let r = send_body(link_app(&s), &case.method, &path, &h, &case.body).await;
            let status = r.status();
            if case.method != "HEAD" {
                let v = json_of(r).await;
                assert_eq!(
                    status,
                    if case.allowed {
                        StatusCode::OK
                    } else {
                        StatusCode::FORBIDDEN
                    },
                    "{} {path}: {v}",
                    case.method
                );
            } else {
                assert_eq!(
                    status,
                    if case.allowed {
                        StatusCode::OK
                    } else {
                        StatusCode::FORBIDDEN
                    },
                    "HEAD {path}"
                );
            }
            if let Some(before) = before {
                assert_eq!(
                    link_approval_snapshot(&s).await,
                    before,
                    "denied approval changed state: {path}"
                );
                approvals.push((path, case.body));
            }
        }
    }
    // The same real ToolRun and GitHub hold questions still work for the
    // ordinary operator. Do this last so all held-card cases see a hold.
    for (path, body) in approvals {
        let r = send_body(
            link_app(&s),
            "POST",
            &path,
            &[("authorization", "Bearer t")],
            &body,
        )
        .await;
        let status = r.status();
        let v = json_of(r).await;
        assert_eq!(status, StatusCode::OK, "operator {path}: {v}");
        assert_eq!(v["status"], "answered");
    }
}

#[tokio::test]
async fn link_card_wait_routes_deny_all_methods() {
    let dir = tempfile::tempdir().unwrap();
    let s = state_at(dir.path());
    seed_sysadmin(&s).await;
    let (_, key) = paired(&s, false).await;
    let auth = format!("Bearer {key}");
    let h = [("authorization", auth.as_str())];
    let (_, registry) = routes_with_registry(s.clone(), false, false);
    for method in ["GET", "PUT", "DELETE"] {
        assert_eq!(registry.access(&method.parse().unwrap(), "/api/cards/P-1/wait-on"), Some(link_registry::Access::OperatorOnly));
        let response = send_body(link_app(&s), method, "/api/cards/P-1/wait-on", &h,
            &serde_json::json!({"dependencies":[{"card":"P-2"}]})).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method}");
    }
}

#[tokio::test]
async fn link_unregistered_routes_never_reach_handlers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let s = state_at(dir.path());
    let (_, key) = paired(&s, false).await;
    let called = Arc::new(AtomicUsize::new(0));
    let app = || {
        let called = called.clone();
        let shadow_called = called.clone();
        let outside_called = called.clone();
        // No registration helper: this could equally come from a macro,
        // a cfg'd helper, or a nested/merged axum router.
        let plain = Router::new().route(
            "/hidden",
            axum::routing::get(move || async move {
                called.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }),
        );
        let (real, registry) = routes_with_registry(s.clone(), false, false);
        let mut auth = TokenAuth::new("t".into(), false).with_links(LinkAuth {
            db: s.db.clone(),
            runtime: s.links.clone(),
        });
        auth.registry = registry;
        let real = real.nest("/api/unregistered", plain).route(
            "/api/agents/shadow",
            axum::routing::get(move || async move {
                shadow_called.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }),
        );
        let real = real.route(
            "/outside",
            axum::routing::get(move || async move {
                outside_called.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }),
        );
        authenticated_router(s.clone(), real, auth)
    };
    let bearer = format!("Bearer {key}");
    let response = send(
        app(),
        "GET",
        "/api/unregistered/hidden",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(called.load(Ordering::SeqCst), 0);
    let response = send(
        app(),
        "GET",
        "/api/agents/shadow",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(called.load(Ordering::SeqCst), 0);
    let response = send(
        app(),
        "GET",
        "/api/unregistered/hidden",
        &[("authorization", "Bearer t")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert_eq!(
        send(app(), "GET", "/outside", &[("authorization", &bearer)])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert_eq!(
        ws(
            app(),
            "/outside",
            &[("sec-websocket-protocol", &format!("bearer.{key}"))]
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert_eq!(
        send(app(), "GET", "/outside", &[]).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(app(), "GET", "/outside", &[("authorization", "Bearer t")])
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(called.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn link_credentials_do_not_bypass_public_routes() {
    let dir = tempfile::tempdir().unwrap();
    let s = state_at(dir.path());
    seed_sysadmin(&s).await;
    let (_, key) = paired(&s, false).await;
    let app = || {
        tcp_router_with(
            s.clone(),
            TokenAuth::new("t".into(), false).with_links(LinkAuth {
                db: s.db.clone(),
                runtime: s.links.clone(),
            }),
            true,
        )
    };
    let op = [("authorization", "Bearer t")];
    let upload = send_body(app(), "POST", "/api/projects/p/artifacts", &op,
        &serde_json::json!({"name":"public.txt", "content_type":"text/plain", "data_base64":"cHVibGlj"})).await;
    assert_eq!(upload.status(), StatusCode::CREATED);
    let artifact = json_of(upload).await;
    let mint = send_body(
        app(),
        "POST",
        &format!("/api/artifacts/{}/url", artifact["id"].as_str().unwrap()),
        &op,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(mint.status(), StatusCode::OK);
    let signed = json_of(mint).await;
    let uri: axum::http::Uri = signed["url"].as_str().unwrap().parse().unwrap();
    let artifact_path = uri.path_and_query().unwrap().as_str();
    let bearer = format!("Bearer {key}");
    for path in [
        "/install/host.sh",
        "/install/terrarium-host",
        "/install/terrarium-host.sha256",
        "/install/terrarium-host.sigstore.json",
        "/",
        "/favicon.svg",
        "/agents/dashboard",
        "/missing.js",
        "/ra/cap/123/sig/",
        "/ra/cap/123/sig/index.html",
        artifact_path,
    ] {
        let anonymous = send(app(), "GET", path, &[]).await;
        let status = anonymous.status();
        assert_ne!(status, StatusCode::UNAUTHORIZED, "public control {path}");
        assert_eq!(
            send(app(), "GET", path, &op).await.status(),
            status,
            "operator control {path}"
        );
        for method in ["GET", "HEAD"] {
            let response = send(app(), method, path, &[("authorization", &bearer)]).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
    assert_eq!(
        send(app(), "GET", artifact_path, &[]).await.status(),
        StatusCode::OK
    );
    assert_eq!(send(app(), "GET", "/", &[]).await.status(), StatusCode::OK);
    assert_eq!(
        send(app(), "GET", "/install/host.sh", &[]).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(app(), "GET", "/", &[("authorization", "Bearer tlk_wrong")])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn link_hidden_methods_in_helpers_are_not_registered() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let s = state_at(dir.path());
    let (_, key) = paired(&s, false).await;
    let called = Arc::new(AtomicUsize::new(0));
    let app = || {
        let called = called.clone();
        // A helper silently adding POST cannot extend the GET declaration.
        let methods = axum::routing::get(|| async { StatusCode::OK }).post(move || async move {
            called.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        });
        let child = link_registry::RegisteredRouter::new().route(
            "/declared",
            methods,
            &[(axum::http::Method::GET, link_registry::Access::Link)],
        );
        let (router, registry) = link_registry::RegisteredRouter::new()
            .nest("/api", child)
            .into_parts();
        let mut auth = TokenAuth::new("t".into(), false).with_links(LinkAuth {
            db: s.db.clone(),
            runtime: s.links.clone(),
        });
        auth.registry = Arc::new(registry);
        authenticated_router(s.clone(), router.with_state(s.clone()), auth)
    };
    let bearer = format!("Bearer {key}");
    let h = [("authorization", bearer.as_str())];
    assert_eq!(
        send(app(), "GET", "/api/declared", &h).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(app(), "HEAD", "/api/declared", &h).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        send(app(), "POST", "/api/declared", &h).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert_eq!(
        send(
            app(),
            "POST",
            "/api/declared",
            &[("authorization", "Bearer t")]
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(called.load(Ordering::SeqCst), 1);
}

#[test]
fn link_remote_artifact_registrations_keep_their_access_classes() {
    let (_, registry) = routes_with_registry(state(), true, false);
    assert_eq!(
        registry.access(
            &axum::http::Method::POST,
            "/api/remotes/remote/artifact-repin"
        ),
        Some(link_registry::Access::OperatorOnly)
    );
    assert_eq!(
        registry.access(
            &axum::http::Method::POST,
            "/api/remotes/remote/artifacts/ART-1/url"
        ),
        Some(link_registry::Access::OperatorOnly)
    );
    for path in ["/ra/cap/123/sig/", "/ra/cap/123/sig/index.html"] {
        for method in [axum::http::Method::GET, axum::http::Method::HEAD] {
            assert_eq!(
                registry.access(&method, path),
                Some(link_registry::Access::PublicDenied)
            );
        }
    }
}

#[test]
fn link_job_log_cleanup_registration_keeps_project_policy() {
    let (_, registry) = routes_with_registry(state(), false, false);
    assert_eq!(
        registry.access(&axum::http::Method::POST, "/api/projects/p/jobs/cleanup"),
        Some(link_registry::Access::OperatorOnly)
    );
}

#[test]
fn link_read_positions_registration_is_operator_only() {
    let (_, registry) = routes_with_registry(state(), false, false);
    assert_eq!(
        registry.access(&axum::http::Method::POST, "/api/read-positions/thread/item"),
        Some(link_registry::Access::OperatorOnly)
    );
}
