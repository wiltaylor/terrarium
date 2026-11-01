//! The store of the DB: schema changes, refs, unique fields, revisions,
//! and queries on real rows.

use super::*;
use crate::error::Code;

fn db() -> Connection {
    let mut c = Connection::open_in_memory().unwrap();
    crate::db::migrate(&mut c).unwrap();
    c.execute_batch(
        "INSERT INTO projects (name, created_at, updated_at, prefix) VALUES ('p', 'x', 'x', 'P');
         INSERT INTO projects (name, created_at, updated_at, prefix) VALUES ('q', 'x', 'x', 'Q');",
    )
    .unwrap();
    c
}

const OP: Actor = Actor::Operator;

/// Run `f` in a transaction and commit it.
fn w<T>(c: &mut Connection, f: impl FnOnce(&mut Tx<'_>) -> ApiResult<T>) -> ApiResult<T> {
    let mut t = Tx::for_test(c.transaction().unwrap(), "operator");
    let v = f(&mut t)?;
    t.commit_for_test().unwrap();
    Ok(v)
}

fn new_type(c: &mut Connection, name: &str, fields: Value) -> DbType {
    let n = NewType {
        name: name.into(),
        description: String::new(),
        fields: serde_json::from_value(fields).unwrap(),
    };
    w(c, |t| create_type(t, &OP, "p", n)).unwrap()
}

fn rec(c: &mut Connection, ty: &str, data: Value) -> ApiResult<Record> {
    let n = NewRecord {
        type_name: ty.into(),
        data: data.as_object().unwrap().clone(),
    };
    w(c, |t| create_record(t, &OP, "p", n))
}

fn patch_type(c: &mut Connection, name: &str, body: Value) -> ApiResult<DbType> {
    let p: TypePatch = serde_json::from_value(body).unwrap();
    w(c, |t| update_type(t, &OP, "p", name, p))
}

fn q(c: &Connection, query: &str) -> ApiResult<QueryResult> {
    run_query(c, &OP, "p", query, None, None)
}

fn services(c: &mut Connection) {
    new_type(
        c,
        "service",
        json!([
            {"name": "name", "kind": "string", "required": true, "unique": true},
            {"name": "port", "kind": "integer"}
        ]),
    );
    for (n, p) in [("auth", 8080), ("billing", 8081), ("search", 9200)] {
        rec(c, "service", json!({"name": n, "port": p})).unwrap();
    }
}

fn bugs(c: &mut Connection) {
    services(c);
    new_type(
        c,
        "bug",
        json!([
            {"name": "title", "kind": "string", "required": true},
            {"name": "severity", "kind": "enum", "required": true, "values": ["low", "medium", "high", "critical"]},
            {"name": "status", "kind": "enum", "values": ["open", "fixed"], "default": "open"},
            {"name": "service", "kind": "ref", "ref": "service"},
            {"name": "reported", "kind": "date"},
            {"name": "points", "kind": "number"},
            {"name": "tags", "kind": "list", "of": "string"}
        ]),
    );
    for (t, s, svc, d, p, tags) in [
        (
            "Login fails",
            "critical",
            "auth",
            "2026-10-01",
            5.0,
            json!(["ui", "auth"]),
        ),
        (
            "Slow invoices",
            "medium",
            "billing",
            "2026-10-03",
            3.0,
            json!(["perf"]),
        ),
        (
            "Typo on page",
            "low",
            "auth",
            "2026-10-05",
            1.0,
            json!(["ui"]),
        ),
        ("Index lost", "high", "search", "2026-10-07", 8.0, json!([])),
    ] {
        rec(
            c,
            "bug",
            json!({"title": t, "severity": s, "service": svc, "reported": d, "points": p, "tags": tags}),
        )
        .unwrap();
    }
}

fn titles(r: &QueryResult) -> Vec<String> {
    r.records
        .iter()
        .map(|x| x.data["title"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn types_create_list_and_refuse_bad_ones() {
    let mut c = db();
    services(&mut c);
    let list = list_types(&c, &OP, "p", &Page::default()).unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].records, 3);
    assert_eq!(list.items[0].revision, 1);
    let bad = |c: &mut Connection, name: &str, fields: Value| {
        let n = NewType {
            name: name.into(),
            description: String::new(),
            fields: serde_json::from_value(fields).unwrap(),
        };
        w(c, |t| create_type(t, &OP, "p", n)).unwrap_err()
    };
    let e = bad(&mut c, "service", json!([{"name": "a", "kind": "string"}]));
    assert_eq!(e.code, Code::Conflict);
    let e = bad(&mut c, "Bad", json!([{"name": "a", "kind": "string"}]));
    assert_eq!(e.code, Code::Invalid);
    let e = bad(
        &mut c,
        "x",
        json!([{"name": "s", "kind": "ref", "ref": "nope"}]),
    );
    assert!(e.message.contains("has no type `nope`"), "{}", e.message);
    // A ref to the type itself is good.
    new_type(
        &mut c,
        "node",
        json!([{"name": "name", "kind": "string"}, {"name": "parent", "kind": "ref", "ref": "node"}]),
    );
    // The other project does not see them.
    assert!(
        list_types(&c, &OP, "q", &Page::default())
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn records_check_refs_unique_and_revisions() {
    let mut c = db();
    bugs(&mut c);
    // The default fills `status`; the service name becomes the record ID.
    let r = rec(
        &mut c,
        "bug",
        json!({"title": "X", "severity": "low", "service": "billing"}),
    )
    .unwrap();
    assert_eq!(r.data["status"], "open");
    let billing = q(&c, "type:service name=billing").unwrap().records[0]
        .id
        .clone();
    assert_eq!(r.data["service"], billing.as_str());
    // A dangling ref, by ID and by name.
    let e = rec(
        &mut c,
        "bug",
        json!({"title": "X", "severity": "low", "service": "01J00000000000000000000000"}),
    )
    .unwrap_err();
    assert_eq!(e.code, Code::Invalid);
    assert!(
        e.details.as_ref().unwrap()["fields"]["service"]
            .as_str()
            .unwrap()
            .contains("no `service` record has the ID")
    );
    let e = rec(
        &mut c,
        "bug",
        json!({"title": "X", "severity": "low", "service": "payments"}),
    )
    .unwrap_err();
    assert!(
        e.message
            .contains("no `service` record has name `payments`"),
        "{}",
        e.message
    );
    // Unique.
    let e = rec(&mut c, "service", json!({"name": "auth"})).unwrap_err();
    assert!(e.message.contains("name: must be unique"), "{}", e.message);
    // Per-field problems.
    let e = rec(&mut c, "bug", json!({"severity": "urgent", "colour": 1})).unwrap_err();
    let f = &e.details.as_ref().unwrap()["fields"];
    assert_eq!(f["title"], "is required");
    assert!(
        f["severity"]
            .as_str()
            .unwrap()
            .starts_with("must be one of")
    );
    assert!(f["colour"].as_str().unwrap().starts_with("is not a field"));

    // A merge: null removes an optional field, a required one stays.
    let p = |data: Value, revision: Option<i64>| RecordPatch {
        data: data.as_object().unwrap().clone(),
        revision,
    };
    let id = r.id.clone();
    let r2 = w(&mut c, |t| {
        update_record(
            t,
            &OP,
            "p",
            &id,
            p(json!({"service": null, "points": 2}), Some(1)),
        )
    })
    .unwrap();
    assert_eq!(r2.revision, 2);
    assert!(!r2.data.contains_key("service"));
    assert_eq!(r2.data["points"], 2);
    let e = w(&mut c, |t| {
        update_record(t, &OP, "p", &id, p(json!({"title": "Y"}), Some(1)))
    })
    .unwrap_err();
    assert_eq!(e.code, Code::Conflict);
    assert_eq!(e.details.as_ref().unwrap()["current"], 2);
    let e = w(&mut c, |t| {
        update_record(t, &OP, "p", &id, p(json!({"title": null}), None))
    })
    .unwrap_err();
    assert_eq!(
        e.details.as_ref().unwrap()["fields"]["title"],
        "is required"
    );
    // No change: no new revision.
    let r3 = w(&mut c, |t| {
        update_record(t, &OP, "p", &id, p(json!({"points": 2}), None))
    })
    .unwrap();
    assert_eq!(r3.revision, 2);
    // The record is not in the other project.
    assert_eq!(
        get_record(&c, &OP, "q", &id).unwrap_err().code,
        Code::NotFound
    );
}

#[test]
fn a_referenced_record_and_type_stay() {
    let mut c = db();
    bugs(&mut c);
    let auth = q(&c, "type:service name=auth").unwrap().records[0]
        .id
        .clone();
    let e = w(&mut c, |t| delete_record(t, &OP, "p", &auth)).unwrap_err();
    assert_eq!(e.code, Code::Conflict);
    let d = e.details.unwrap();
    assert_eq!(d["total"], 2);
    assert_eq!(d["referrers"][0]["type"], "bug");
    assert_eq!(d["referrers"][0]["field"], "service");
    // The type is a target.
    let e = w(&mut c, |t| {
        delete_type(t, &OP, "p", "service", Some("records"))
    })
    .unwrap_err();
    assert!(e.message.contains("target of bug.service"), "{}", e.message);
    // A record that nothing points at goes.
    let search = q(&c, "type:service name=search").unwrap().records[0]
        .id
        .clone();
    let bug = q(&c, "type:bug service=search").unwrap().records[0]
        .id
        .clone();
    w(&mut c, |t| delete_record(t, &OP, "p", &bug)).unwrap();
    w(&mut c, |t| delete_record(t, &OP, "p", &search)).unwrap();
    // Type deletes need a confirm.
    let e = w(&mut c, |t| delete_type(t, &OP, "p", "bug", None)).unwrap_err();
    assert_eq!(e.code, Code::ConfirmRequired);
    assert_eq!(e.details.unwrap()["records"], 3);
    let e = w(&mut c, |t| delete_type(t, &OP, "p", "bug", Some("true"))).unwrap_err();
    assert_eq!(e.code, Code::ConfirmRequired);
    w(&mut c, |t| delete_type(t, &OP, "p", "bug", Some("records"))).unwrap();
    let n: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM db_records WHERE project = 'p'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2);
    new_type(&mut c, "empty", json!([{"name": "a", "kind": "string"}]));
    let e = w(&mut c, |t| delete_type(t, &OP, "p", "empty", None)).unwrap_err();
    assert_eq!(e.code, Code::ConfirmRequired);
    w(&mut c, |t| delete_type(t, &OP, "p", "empty", Some("true"))).unwrap();
}

#[test]
fn schema_changes() {
    let mut c = db();
    bugs(&mut c);
    let fields = |c: &Connection| get_type(c, &OP, "p", "bug").unwrap().fields;
    let mut f: Vec<Value> = fields(&c)
        .iter()
        .map(|x| serde_json::to_value(x).unwrap())
        .collect();

    // Add an optional field: free.
    f.push(json!({"name": "owner", "kind": "string"}));
    let t = patch_type(&mut c, "bug", json!({"revision": 1, "fields": f})).unwrap();
    assert_eq!(t.revision, 2);
    // A stale revision.
    let e = patch_type(&mut c, "bug", json!({"revision": 1, "description": "x"})).unwrap_err();
    assert_eq!(e.code, Code::Conflict);

    // A required field needs a default when there are records; it fills them.
    f.push(json!({"name": "team", "kind": "string", "required": true}));
    let e = patch_type(&mut c, "bug", json!({"revision": 2, "fields": f})).unwrap_err();
    assert!(
        e.message.contains("needs a default for the 4 records"),
        "{}",
        e.message
    );
    f.pop();
    f.push(json!({"name": "team", "kind": "string", "required": true, "default": "core"}));
    let t = patch_type(&mut c, "bug", json!({"revision": 2, "fields": f})).unwrap();
    assert_eq!(t.revision, 3);
    let r = q(&c, "type:bug team=core").unwrap();
    assert_eq!(r.count, 4);
    assert_eq!(r.records[0].revision, 2);

    // A kind change that the records fail is refused, with the records.
    let i = f.iter().position(|x| x["name"] == "title").unwrap();
    f[i]["kind"] = json!("integer");
    let e = patch_type(&mut c, "bug", json!({"revision": 3, "fields": f})).unwrap_err();
    assert_eq!(e.code, Code::Invalid);
    let d = e.details.unwrap();
    assert_eq!(d["failing"], 4);
    assert_eq!(d["records"][0]["field"], "title");
    assert_eq!(d["records"][0]["message"], "must be an integer");
    f[i]["kind"] = json!("string");
    // integer → number passes; making an optional field required that a record lacks fails.
    let i = f.iter().position(|x| x["name"] == "owner").unwrap();
    f[i]["required"] = json!(true);
    let e = patch_type(&mut c, "bug", json!({"revision": 3, "fields": f})).unwrap_err();
    assert!(
        e.message.contains("4 records of `bug` do not fit"),
        "{}",
        e.message
    );
    f[i]["required"] = json!(false);

    // Remove a field: confirm, and the data goes.
    let g: Vec<Value> = f
        .iter()
        .filter(|x| x["name"] != "points")
        .cloned()
        .collect();
    let e = patch_type(&mut c, "bug", json!({"revision": 3, "fields": g})).unwrap_err();
    assert_eq!(e.code, Code::ConfirmRequired);
    assert_eq!(e.details.unwrap()["removed"], json!(["points"]));
    let t = patch_type(
        &mut c,
        "bug",
        json!({"revision": 3, "fields": g, "confirm": true}),
    )
    .unwrap();
    assert_eq!(t.revision, 4);
    let r = q(&c, "type:bug").unwrap();
    assert!(r.records.iter().all(|x| !x.data.contains_key("points")));

    // Rename: the data moves.
    let t = patch_type(
        &mut c,
        "bug",
        json!({"revision": 4, "rename": [{"from": "reported", "to": "found_on"}]}),
    )
    .unwrap();
    assert!(t.fields.iter().any(|x| x.name == "found_on"));
    let r = q(&c, "type:bug found_on>=2026-10-05 sort:found_on").unwrap();
    assert_eq!(titles(&r), ["Typo on page", "Index lost"]);
    let e = patch_type(
        &mut c,
        "bug",
        json!({"revision": 5, "rename": [{"from": "nope", "to": "x"}]}),
    )
    .unwrap_err();
    assert!(e.message.contains("no such field"));
    let e = patch_type(
        &mut c,
        "bug",
        json!({"revision": 5, "rename": [{"from": "title", "to": "team"}]}),
    )
    .unwrap_err();
    assert!(e.message.contains("has a field `team`"));
    // Turning on unique with duplicates fails.
    let mut h: Vec<Value> = get_type(&c, &OP, "p", "bug")
        .unwrap()
        .fields
        .iter()
        .map(|x| serde_json::to_value(x).unwrap())
        .collect();
    let i = h.iter().position(|x| x["name"] == "team").unwrap();
    h[i]["unique"] = json!(true);
    h[i]["default"] = Value::Null;
    let e = patch_type(&mut c, "bug", json!({"revision": 5, "fields": h})).unwrap_err();
    assert!(e.message.contains("must be unique"), "{}", e.message);
}

#[test]
fn queries_on_rows() {
    let mut c = db();
    bugs(&mut c);
    // Enum order, both ways.
    let r = q(&c, "type:bug sort:-severity").unwrap();
    assert_eq!(
        titles(&r),
        ["Login fails", "Index lost", "Slow invoices", "Typo on page"]
    );
    let r = q(&c, "type:bug severity>=high sort:severity").unwrap();
    assert_eq!(titles(&r), ["Index lost", "Login fails"]);
    // Contains without case; list has; dates; numbers.
    assert_eq!(
        titles(&q(&c, "type:bug title~LOGIN").unwrap()),
        ["Login fails"]
    );
    assert_eq!(q(&c, "type:bug tags~UI").unwrap().count, 2);
    assert_eq!(q(&c, "type:bug tags!=ui").unwrap().count, 2);
    assert_eq!(q(&c, "type:bug reported<2026-10-04").unwrap().count, 2);
    assert_eq!(q(&c, "type:bug points>=3 points<8").unwrap().count, 2);
    assert_eq!(q(&c, "type:bug points=5").unwrap().count, 1);
    // A ref by name, by ID; null checks.
    assert_eq!(q(&c, "type:bug service=auth").unwrap().count, 2);
    assert_eq!(q(&c, "type:bug service!=auth").unwrap().count, 2);
    let auth = q(&c, "type:service name=auth").unwrap().records[0]
        .id
        .clone();
    assert_eq!(q(&c, &format!("type:bug service={auth}")).unwrap().count, 2);
    rec(&mut c, "bug", json!({"title": "Lonely", "severity": "low"})).unwrap();
    assert_eq!(q(&c, "type:bug service:null").unwrap().count, 1);
    assert_eq!(q(&c, "type:bug service:!null").unwrap().count, 4);
    assert_eq!(q(&c, "type:bug service!=auth").unwrap().count, 3);
    // Quoted values.
    assert_eq!(q(&c, r#"type:bug title="Slow invoices""#).unwrap().count, 1);
    // Projection, labels.
    let r = q(
        &c,
        "type:bug fields:title,service,created service=auth sort:title",
    )
    .unwrap();
    assert_eq!(r.fields.len(), 3);
    assert!(r.fields[2].meta);
    assert_eq!(r.records[0].data.len(), 2);
    assert_eq!(r.labels[&auth], "auth");
    // Limit and the cursor.
    let r = q(&c, "type:bug limit:2 sort:title").unwrap();
    assert_eq!(r.records.len(), 2);
    assert_eq!(r.count, 5);
    assert_eq!(r.next_cursor.as_deref(), Some("2"));
    let r2 = run_query(&c, &OP, "p", "type:bug limit:2 sort:title", None, Some("4")).unwrap();
    assert_eq!(titles(&r2), ["Typo on page"]);
    assert_eq!(r2.next_cursor, None);
    // Count, group (enum order, list items, ref labels).
    let r = q(&c, "type:bug count:true").unwrap();
    assert!(r.records.is_empty());
    assert_eq!((r.count, r.view.as_str()), (5, "count"));
    let r = q(&c, "type:bug group:severity").unwrap();
    let g: Vec<(Value, i64)> = r
        .groups
        .unwrap()
        .into_iter()
        .map(|g| (g.value, g.count))
        .collect();
    assert_eq!(
        g,
        [
            (json!("low"), 2),
            (json!("medium"), 1),
            (json!("high"), 1),
            (json!("critical"), 1)
        ]
    );
    let r = q(&c, "type:bug group:tags").unwrap();
    assert_eq!(
        r.groups.unwrap()[0],
        Group {
            value: json!("ui"),
            count: 2
        }
    );
    let r = q(&c, "type:bug group:service").unwrap();
    let groups = r.groups.unwrap();
    assert_eq!(groups[0].count, 2);
    assert_eq!(r.labels[groups[0].value.as_str().unwrap()], "auth");
    // The type scope of the request.
    assert_eq!(
        run_query(&c, &OP, "p", "severity=low", Some("bug"), None)
            .unwrap()
            .count,
        2
    );
    let e = run_query(&c, &OP, "p", "type:service", Some("bug"), None).unwrap_err();
    assert_eq!(e.code, Code::BadRequest);
    // Errors: 400 with the position.
    let e = q(&c, "type:bug sevrity=low").unwrap_err();
    assert_eq!(e.code, Code::BadRequest);
    assert_eq!(e.details.unwrap()["position"], 9);
    let e = q(&c, "type:nope").unwrap_err();
    assert!(
        e.message.contains("its types: bug, service"),
        "{}",
        e.message
    );
    assert_eq!(e.details.unwrap()["position"], 5);
    assert!(
        q(&c, "title=x")
            .unwrap_err()
            .message
            .contains("needs type:")
    );
    // Hostile values are values.
    assert_eq!(q(&c, r#"type:bug title="x' OR '1'='1""#).unwrap().count, 0);
    assert_eq!(q(&c, r#"type:bug title~"%') --""#).unwrap().count, 0);
}

#[test]
fn rights() {
    let mut c = db();
    services(&mut c);
    let agent = |project: &str, roles: Vec<terrarium_core::Role>| {
        Actor::Agent(crate::actor::AgentActor {
            id: "01AGENTAGENTAGENTAGENTAGEN".into(),
            name: "w".into(),
            project: project.into(),
            roles,
            policy: Default::default(),
        })
    };
    let own = agent("p", vec![]);
    let other = agent("q", vec![]);
    let admin = agent("p", vec![terrarium_core::Role::ProjectAdmin]);
    assert!(run_query(&c, &own, "p", "type:service", None, None).is_ok());
    let e = run_query(&c, &other, "p", "type:service", None, None).unwrap_err();
    assert_eq!(e.code, Code::Forbidden);
    let n = || NewRecord {
        type_name: "service".into(),
        data: json!({"name": "new"}).as_object().unwrap().clone(),
    };
    assert_eq!(
        w(&mut c, |t| create_record(t, &other, "p", n()))
            .unwrap_err()
            .code,
        Code::Forbidden
    );
    w(&mut c, |t| create_record(t, &own, "p", n())).unwrap();
    let e = w(&mut c, |t| {
        delete_type(t, &own, "p", "service", Some("records"))
    })
    .unwrap_err();
    assert_eq!(e.code, Code::Forbidden);
    assert!(
        e.message.contains("deleting a DB type with its 4 records"),
        "{}",
        e.message
    );
    w(&mut c, |t| {
        delete_type(t, &admin, "p", "service", Some("records"))
    })
    .unwrap();
}
