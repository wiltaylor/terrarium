// Runtime registry entries are the boundary. These additional reviewed cases
// exercise conditional actions and real approval resources.
#[derive(serde::Deserialize)]
struct LinkRouteInventory { routes: Vec<LinkRouteEntry> }
#[derive(serde::Deserialize)]
struct LinkRouteEntry { cases: Vec<LinkRouteCase> }
#[derive(serde::Deserialize)]
struct LinkRouteCase {
    method: String,
    path: String,
    #[serde(default)] body: serde_json::Value,
    #[serde(default)] allowed: bool,
    #[serde(default)] approval: bool,
}
fn link_inventory() -> LinkRouteInventory {
    serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/link-route-access.json"))).unwrap()
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
        if entry.access != link_registry::Access::OperatorOnly { continue; }
        let path = entry.path.split('/').map(|segment| {
            if segment.starts_with('{') { "link-test" } else { segment }
        }).collect::<Vec<_>>().join("/");
        let response = send_body(link_app(&s), entry.method.as_str(), &path, &h, &serde_json::json!({})).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{} {path}", entry.method);
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
async fn link_unregistered_routes_never_reach_handlers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let s = state_at(dir.path());
    let (_, key) = paired(&s, false).await;
    let called = Arc::new(AtomicUsize::new(0));
    let app = || {
        let called = called.clone();
        // No registration helper: this could equally come from a macro,
        // a cfg'd helper, or a nested/merged axum router.
        let plain = Router::new().route("/hidden", get(move || async move {
            called.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }));
        let (real, registry) = routes_with_registry(s.clone(), false, false);
        let mut auth = TokenAuth::new("t".into(), false).with_links(LinkAuth { db: s.db.clone(), runtime: s.links.clone() });
        auth.registry = registry;
        authenticated_router(s.clone(), real.nest("/api/unregistered", plain), auth)
    };
    let bearer = format!("Bearer {key}");
    let response = send(app(), "GET", "/api/unregistered/hidden", &[("authorization", &bearer)]).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(called.load(Ordering::SeqCst), 0);
    let response = send(app(), "GET", "/api/unregistered/hidden", &[("authorization", "Bearer t")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(called.load(Ordering::SeqCst), 1);
}
