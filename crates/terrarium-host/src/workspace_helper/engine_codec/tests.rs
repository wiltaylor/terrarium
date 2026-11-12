use super::*;

fn response(bytes: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        bytes.len()
    )
    .into_bytes();
    r.extend_from_slice(bytes);
    r
}

fn decode(r: &[u8]) -> Result<Inventory, Refused> {
    inventory(
        r,
        Duration::ZERO,
        Identity {
            name: "workspace-one",
            labels: &[
                ("registration", "reg-one"),
                ("incarnation", "inc-one"),
                ("bootstrap", "boot-one"),
            ],
        },
    )
}

fn record() -> Value {
    serde_json::json!({"Id": "a".repeat(64), "Names": ["/foreign"], "Labels": {}, "State": "exited"})
}

fn records(v: Vec<Value>) -> Vec<u8> {
    response(&serde_json::to_vec(&v).unwrap())
}

#[test]
fn fixed_request_is_unfiltered_all_containers() {
    assert_eq!(INVENTORY_REQUEST, b"GET /v1.47/containers/json?all=1&size=0 HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
}

#[test]
fn empty_complete_array_is_distinct_from_empty_body_or_error() {
    assert_eq!(
        decode(&response(b"[]")),
        Ok(Inventory {
            records: 0,
            conflicting_ids: vec![]
        })
    );
    for b in [
        b"".as_slice(),
        b"null",
        b"{}",
        b"[",
        b"[] junk",
        b"[][]",
        b"[null]",
        b"\xff",
    ] {
        assert_eq!(decode(&response(b)), Err(Refused));
    }
    for status in [
        "404 Not Found",
        "500 Internal Server Error",
        "206 Partial Content",
        "301 Moved Permanently",
        "204 No Content",
    ] {
        let r = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]"
        );
        assert_eq!(decode(r.as_bytes()), Err(Refused));
    }
}

#[test]
fn stopped_conflicts_match_each_name_and_each_identity_label_independently() {
    for state in [
        "created",
        "exited",
        "dead",
        "paused",
        "running",
        "restarting",
        "removing",
    ] {
        for selector in 0..4 {
            let mut r = record();
            r["State"] = state.into();
            match selector {
                0 => r["Names"] = serde_json::json!(["/foreign", "/workspace-one"]),
                1 => {
                    r["Labels"] = serde_json::json!({"registration": "reg-one", "incarnation": "wrong", "bootstrap": "wrong"})
                }
                2 => {
                    r["Labels"] = serde_json::json!({"registration": "wrong", "incarnation": "inc-one", "bootstrap": "wrong"})
                }
                _ => {
                    r["Labels"] = serde_json::json!({"registration": "wrong", "incarnation": "wrong", "bootstrap": "boot-one"})
                }
            }
            assert_eq!(
                decode(&records(vec![r])).unwrap().conflicting_ids,
                vec!["a".repeat(64)]
            );
        }
    }
    assert!(
        decode(&records(vec![record()]))
            .unwrap()
            .conflicting_ids
            .is_empty()
    );
}

#[test]
fn validates_entire_inventory_even_after_conflict() {
    let mut first = record();
    first["Names"] = serde_json::json!(["/workspace-one"]);
    let mut last = record();
    last["Id"] = "b".repeat(64).into();
    last["Labels"] = Value::Null;
    assert_eq!(decode(&records(vec![first, last])), Err(Refused));
}

#[test]
fn framing_truncation_trailing_duplicates_and_overflow_refuse() {
    let good = response(b"[]");
    for end in 0..good.len() {
        assert_eq!(decode(&good[..end]), Err(Refused));
    }
    let mut trailing = good;
    trailing.extend_from_slice(b"HTTP/1.1 200 OK\r\n\r\n");
    assert_eq!(decode(&trailing), Err(Refused));
    for headers in [
        "Content-Length: 2\r\ncontent-length: 2",
        "Content-Length: 2\r\nTransfer-Encoding: chunked",
        "Content-Length: 2\r\nContent-Encoding: gzip",
        "Content-Length: 2\r\nContent-Range: bytes 0-1/20",
        "Content-Length: 2\r\nTrailer: x",
        "Content-Length: 184467440737095516160000",
        "Content-Length: +2",
        "Content-Length: -2",
        "Content-Length: 3",
        "Content-Length: 1",
        "X: y",
        " Content-Length: 2",
        "Content-Length: 2\r\n folded",
        "Content-Length: 2\r\nX: bad\nvalue",
    ] {
        let r = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{headers}\r\n\r\n[]");
        assert_eq!(decode(r.as_bytes()), Err(Refused), "{headers}");
    }
}

#[test]
fn deadline_and_finite_bounds_refuse() {
    let identity = || Identity {
        name: "n",
        labels: &[("k", "v")],
    };
    assert!(
        inventory(
            &response(b"[]"),
            DEADLINE - Duration::from_nanos(1),
            identity()
        )
        .is_ok()
    );
    assert_eq!(
        inventory(&response(b"[]"), DEADLINE, identity()),
        Err(Refused)
    );
    assert_eq!(decode(&response(&vec![b' '; MAX_BODY + 1])), Err(Refused));
    let r = format!(
        "HTTP/1.1 200 OK\r\nX: {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]",
        "a".repeat(MAX_HEADERS)
    );
    assert_eq!(decode(r.as_bytes()), Err(Refused));
    let mut v = record();
    v["Labels"] = serde_json::json!({"x": "a".repeat(MAX_STRING + 1)});
    assert_eq!(decode(&records(vec![v])), Err(Refused));
    assert_eq!(
        decode(&records(vec![record(); MAX_RECORDS + 1])),
        Err(Refused)
    );
    let mut v = record();
    v["Extra"] = serde_json::from_str(&format!("{}0{}", "[".repeat(40), "]".repeat(40))).unwrap();
    assert_eq!(decode(&records(vec![v])), Err(Refused));
    let deep = format!("{}0{}", "[".repeat(140), "]".repeat(140));
    assert_eq!(decode(&response(deep.as_bytes())), Err(Refused));
}

#[test]
fn duplicate_ids_fields_labels_and_unknown_nested_fields_refuse() {
    assert_eq!(decode(&records(vec![record(), record()])), Err(Refused));
    for fields in [
        "\"Labels\":{},\"Labels\":{}",
        "\"Labels\":{\"x\":\"1\",\"x\":\"2\"}",
        "\"Labels\":{},\"unknown\":{\"x\":1,\"x\":2}",
        "\"Labels\":{},\"State\":\"running\"",
    ] {
        let b = format!(
            "[{{\"Id\":\"{}\",\"Names\":[\"/n\"],\"State\":\"exited\",{fields}}}]",
            "a".repeat(64)
        );
        assert_eq!(decode(&response(b.as_bytes())), Err(Refused));
    }
}

#[test]
fn missing_or_malformed_identity_fields_refuse() {
    for key in ["Id", "Names", "Labels", "State"] {
        let mut r = record();
        r.as_object_mut().unwrap().remove(key);
        assert_eq!(decode(&records(vec![r])), Err(Refused));
    }
    for (key, value) in [
        ("Id", serde_json::json!("short")),
        ("Id", serde_json::json!("A".repeat(64))),
        ("Names", serde_json::json!([])),
        ("Names", serde_json::json!(["/n", "/n"])),
        ("Names", serde_json::json!(["n"])),
        ("Names", serde_json::json!(["//n"])),
        ("Names", serde_json::json!(["/n/other"])),
        ("Names", serde_json::json!([1])),
        ("Labels", serde_json::json!({"x":1})),
        ("Labels", Value::Null),
        ("State", serde_json::json!("unknown")),
    ] {
        let mut r = record();
        r[key] = value;
        assert_eq!(decode(&records(vec![r])), Err(Refused));
    }
}

#[test]
fn malformed_selector_cannot_report_absence() {
    for name in ["", "/workspace-one", ".hidden", "-bad", "with space"] {
        assert_eq!(
            inventory(
                &response(b"[]"),
                Duration::ZERO,
                Identity {
                    name,
                    labels: &[("k", "v")]
                }
            ),
            Err(Refused)
        );
    }
    for labels in [
        &[][..],
        &[("k", "")][..],
        &[("", "v")][..],
        &[("k", "one"), ("k", "two")][..],
    ] {
        assert_eq!(
            inventory(
                &response(b"[]"),
                Duration::ZERO,
                Identity { name: "n", labels }
            ),
            Err(Refused)
        );
    }
}

#[test]
fn record_bound_is_inclusive_and_overflow_is_not_partial_absence() {
    let mut rows = Vec::new();
    for n in 0..MAX_RECORDS {
        let mut r = record();
        r["Id"] = format!("{n:064x}").into();
        rows.push(r);
    }
    assert_eq!(decode(&records(rows.clone())).unwrap().records, MAX_RECORDS);
    let mut extra = record();
    extra["Id"] = format!("{:064x}", MAX_RECORDS).into();
    extra["Names"] = serde_json::json!(["/workspace-one"]);
    rows.push(extra);
    assert_eq!(decode(&records(rows)), Err(Refused));
}

#[test]
fn retained_utf8_whitespace_and_escaping_are_exact_in_decoded_security_opt() {
    // Synthetic opaque JSON, not an approved or runnable profile.
    let bytes = b" \n{ \"fixture\": \"quote=\\\" slash=\\\\ unicode=\\u00e9\", \"literal\": \"caf\xc3\xa9\" }\t";
    let policy = PolicyBytes::retain(bytes).unwrap();
    assert_eq!(policy.sha256(), <[u8; 32]>::from(Sha256::digest(bytes)));
    let fragment: Value = serde_json::from_slice(&policy.encode_fragment()).unwrap();
    let opts = &fragment["HostConfig"]["SecurityOpt"];
    assert_eq!(
        opts[1]
            .as_str()
            .unwrap()
            .strip_prefix("seccomp=")
            .unwrap()
            .as_bytes(),
        bytes
    );
    assert_eq!(
        policy.inspect_parity(&serde_json::to_vec(opts).unwrap()),
        Ok(())
    );
    let equivalent: Value = serde_json::from_slice(bytes).unwrap();
    let compact = PolicyBytes::retain(&serde_json::to_vec(&equivalent).unwrap()).unwrap();
    assert_ne!(policy.sha256(), compact.sha256());
    let different: Value = serde_json::from_slice(&compact.encode_fragment()).unwrap();
    assert_eq!(
        policy
            .inspect_parity(&serde_json::to_vec(&different["HostConfig"]["SecurityOpt"]).unwrap()),
        Err(Refused)
    );
}

#[test]
fn policy_and_inspect_errors_never_equal_parity() {
    for bytes in [
        b"".as_slice(),
        b"null",
        b"[]",
        b"{}junk",
        b"{\"x\":1,\"x\":2}",
        b"{\"x\":\"\xff\"}",
    ] {
        assert!(PolicyBytes::retain(bytes).is_err());
    }
    assert!(PolicyBytes::retain(&vec![b' '; MAX_POLICY + 1]).is_err());
    let p = PolicyBytes::retain(b"{}").unwrap();
    for b in [
        b"".as_slice(),
        b"null",
        b"[]",
        b"{}",
        b"[\"no-new-privileges=true\"]",
        b"[\"seccomp={}\",\"no-new-privileges=true\"]",
        b"[\"no-new-privileges=true\",\"seccomp={}\",\"extra\"]",
        b"[\"no-new-privileges=true\",\"seccomp= {}\"]",
    ] {
        assert_eq!(p.inspect_parity(b), Err(Refused));
    }
}
