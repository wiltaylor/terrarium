// The inventory is a reviewed classification, not a generated allow list.
// New registrations, methods, handlers, and operator comparisons must be
// classified before this test passes. The HTTP walk consumes the same data.
#[derive(serde::Deserialize)]
struct LinkRouteInventory {
    routes: Vec<LinkRouteEntry>,
    gates: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(serde::Deserialize)]
struct LinkRouteEntry {
    source: String,
    registration: String,
    access: String,
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

fn link_inventory_sources(
    dir: &Path,
    root: &Path,
    out: &mut std::collections::BTreeMap<String, String>,
) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            link_inventory_sources(&path, root, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && path.file_name().unwrap() != "link_access_tests.rs"
        {
            let source = std::fs::read_to_string(&path).unwrap();
            out.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                source.split("#[cfg(test)]").next().unwrap().to_owned(),
            );
        }
    }
}

fn link_registration_expressions(source: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = source;
    // Capture the whole registration, including its method chain and handler
    // names. Constants as paths are included too, not silently skipped.
    while let Some(start) = rest.find(".route(") {
        let call = &rest[start + 7..];
        let (mut depth, mut quoted, mut escaped) = (1, false, false);
        let mut end = None;
        for (i, c) in call.char_indices() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    quoted = false;
                }
            } else {
                match c {
                    '"' => quoted = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        let end = end.expect("unparsed route registration");
        result.push(call[..end].chars().filter(|c| !c.is_whitespace()).collect());
        rest = &call[end + 1..];
    }
    result
}

fn link_gate_lines(source: &str) -> Vec<String> {
    let mut result: Vec<_> = source
        .lines()
        .filter(|line| {
            !line.trim_start().starts_with("//")
                && [
                    "Actor::Operator",
                    "Address::Operator",
                    "operator_only(",
                    "operator_only_with_provenance(",
                    ".is_operator()",
                ]
                .iter()
                .any(|needle| line.contains(needle))
        })
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        })
        .collect();
    result.sort();
    result
}

#[test]
fn link_route_inventory_is_complete() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = std::collections::BTreeMap::new();
    link_inventory_sources(&root, &root, &mut sources);
    let inventory = link_inventory();
    let actual: std::collections::BTreeSet<_> = sources
        .iter()
        .flat_map(|(name, source)| {
            link_registration_expressions(source)
                .into_iter()
                .map(|r| (name.clone(), r))
        })
        .collect();
    let expected: std::collections::BTreeSet<_> = inventory
        .routes
        .iter()
        .map(|r| (r.source.clone(), r.registration.clone()))
        .collect();
    assert_eq!(
        actual, expected,
        "classify each new or changed router registration in link-route-access.json"
    );
    assert_eq!(
        expected.len(),
        inventory.routes.len(),
        "duplicate inventory entry"
    );
    for entry in &inventory.routes {
        assert!(
            matches!(
                entry.access.as_str(),
                "operator-only" | "link-allowed-read" | "conditional" | "other"
            ),
            "unclassified registration: {}",
            entry.registration
        );
        if entry.access != "other" {
            assert!(
                !entry.cases.is_empty(),
                "missing HTTP coverage: {}",
                entry.registration
            );
        }
    }
    let gates: std::collections::BTreeMap<_, _> = sources
        .iter()
        .filter_map(|(name, source)| {
            let lines = link_gate_lines(source);
            (!lines.is_empty()).then(|| (name.clone(), lines))
        })
        .collect();
    assert_eq!(
        gates, inventory.gates,
        "review new operator comparisons and conditional gates, then update their route classes and HTTP cases"
    );
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
                on: vec!["idle".into()],
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
    link_route_inventory_is_complete();
    let dir = tempfile::tempdir().unwrap();
    let s = state_at(dir.path());
    let (_, dev) = seed_sysadmin(&s).await;
    let (_, key) = paired(&s, false).await;
    let fixture = link_approval_fixtures(&s, dev).await;
    let auth = format!("Bearer {key}");
    let h = [("authorization", auth.as_str())];
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
