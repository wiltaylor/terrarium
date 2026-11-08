#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

macro_rules! fixture {
    ($f:literal) => {
        include_str!(concat!("fixtures/", $f))
    };
}

fn run(text: &str) -> ParsedRun {
    parse(text.as_bytes(), None).unwrap_or_else(|e| panic!("{e}"))
}

fn case<'a>(r: &'a ParsedRun, name: &str) -> &'a Case {
    r.suites
        .iter()
        .flat_map(|s| &s.cases)
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no case {name}"))
}

fn counts(p: i64, f: i64, e: i64, s: i64) -> Counts {
    Counts {
        passed: p,
        failed: f,
        errored: e,
        skipped: s,
        total: p + f + e + s,
    }
}

fn invalid_msg(r: Result<ParsedRun, ParseError>) -> String {
    match r {
        Err(ParseError::Invalid(m)) => m,
        other => panic!("want invalid, got {other:?}"),
    }
}

// ---- detection ----

#[test]
fn detects_each_fixture() {
    let cases: &[(&str, Format)] = &[
        (fixture!("nextest-junit.xml"), Format::Junit),
        (fixture!("vitest-junit.xml"), Format::Junit),
        (fixture!("pytest-junit.xml"), Format::Junit),
        (fixture!("surefire-nested.xml"), Format::Junit),
        (fixture!("dotnet.trx"), Format::Trx),
        (fixture!("ctrf.json"), Format::Ctrf),
        (fixture!("cargo-libtest.json"), Format::LibtestJson),
        (fixture!("vitest.json"), Format::VitestJson),
        (fixture!("jest.json"), Format::JestJson),
        (fixture!("node.tap"), Format::Tap),
        (fixture!("basic.tap"), Format::Tap),
    ];
    for (text, want) in cases {
        let got = detect(text.trim_start_matches('\u{feff}')).unwrap();
        assert_eq!(got, *want);
        assert_eq!(run(text).format, *want);
    }
}

#[test]
fn detects_single_lines_and_refuses_unknown_content() {
    assert_eq!(
        detect(r#"{"type":"suite","event":"started","test_count":0}"#).unwrap(),
        Format::LibtestJson
    );
    assert_eq!(detect("1..3\nok 1\n").unwrap(), Format::Tap);
    assert_eq!(detect("# comment\nnot ok 1 - x\n").unwrap(), Format::Tap);
    assert!(invalid_msg(parse(b"<html><body/></html>", None)).contains("unknown XML root"));
    assert!(invalid_msg(parse(b"{\"a\": 1}", None)).contains("unknown JSON report"));
    assert!(invalid_msg(parse(b"hello world", None)).contains("cannot tell the format"));
    assert!(invalid_msg(parse(b"{ nope", None)).contains("bad JSON"));
    assert!(invalid_msg(parse(b"   \n ", None)).contains("empty"));
    assert!(invalid_msg(parse(&[0xff, 0xfe, 0xff, 0xd8], None)).contains("UTF-16"));
    assert!(invalid_msg(parse(&[0x80, 0x81], None)).contains("not UTF-8"));
}

#[test]
fn format_names() {
    for f in Format::ALL {
        assert_eq!(Format::parse_name(f.as_str()).unwrap(), Some(f));
        assert_eq!(
            serde_json::to_value(f).unwrap(),
            serde_json::json!(f.as_str())
        );
    }
    assert_eq!(Format::parse_name("auto").unwrap(), None);
    assert_eq!(Format::parse_name("").unwrap(), None);
    assert_eq!(Format::parse_name("JUnit").unwrap(), Some(Format::Junit));
    assert!(Format::parse_name("xunit").is_err());
}

#[test]
fn an_explicit_format_that_does_not_fit_is_invalid() {
    let m = invalid_msg(parse(fixture!("ctrf.json").as_bytes(), Some(Format::Junit)));
    assert!(m.contains("bad XML") || m.contains("no root"), "{m}");
    let m = invalid_msg(parse(fixture!("jest.json").as_bytes(), Some(Format::Ctrf)));
    assert!(m.contains("results"), "{m}");
    let m = invalid_msg(parse(
        fixture!("nextest-junit.xml").as_bytes(),
        Some(Format::Trx),
    ));
    assert!(m.contains("TestRun"), "{m}");
    let m = invalid_msg(parse(b"plain words", Some(Format::Tap)));
    assert!(m.contains("no TAP test point"), "{m}");
    let m = invalid_msg(parse(b"not json at all", Some(Format::LibtestJson)));
    assert!(m.contains("no libtest JSON"), "{m}");
}

// ---- JUnit ----

#[test]
fn junit_nextest() {
    let r = run(fixture!("nextest-junit.xml"));
    assert_eq!(r.tool.as_deref(), Some("cargo-nextest"));
    assert_eq!(r.suggested_name, None);
    assert_eq!(r.counts(), counts(3, 1, 1, 1));
    assert_eq!(r.duration_ms, Some(3207));
    assert_eq!(r.started_at.as_deref(), Some("2026-11-01T22:10:04.512Z"));
    let names: Vec<&str> = r.suites.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["calc", "calc::integration"]);
    let f = case(&r, "tests::adds_negative_numbers");
    assert_eq!(f.status, Status::Failed);
    assert_eq!(f.classname.as_deref(), Some("calc"));
    assert_eq!(f.duration_ms, Some(6));
    assert!(
        f.message
            .as_deref()
            .unwrap()
            .starts_with("thread 'tests::adds_negative_numbers' panicked")
    );
    assert!(f.trace.as_deref().unwrap().contains("right: -3"));
    assert_eq!(f.stdout.as_deref(), Some("checking -1 + -1"));
    assert!(
        f.stderr
            .as_deref()
            .unwrap()
            .contains("panicked at src/lib.rs:10:64")
    );
    // A flaky test passed in the end; its retry is in the trace.
    let flaky = case(&r, "talks_to_the_server");
    assert_eq!(flaky.status, Status::Passed);
    assert!(
        flaky
            .trace
            .as_deref()
            .unwrap()
            .contains("[flakyFailure] connection refused")
    );
    let e = case(&r, "survives_a_restart");
    assert_eq!(e.status, Status::Errored);
    assert_eq!(e.message.as_deref(), Some("test timed out after 60s"));
    assert!(e.trace.as_deref().unwrap().contains("[rerunError]"));
    assert_eq!(case(&r, "tests::adds_many_numbers").status, Status::Skipped);
}

#[test]
fn junit_vitest() {
    let r = run(fixture!("vitest-junit.xml"));
    assert_eq!(r.tool.as_deref(), Some("vitest"));
    assert_eq!(r.counts(), counts(3, 2, 0, 2));
    assert_eq!(r.duration_ms, Some(79));
    let f = case(&r, "cart > totals the prices");
    assert_eq!(f.status, Status::Failed);
    assert_eq!(
        f.message.as_deref(),
        Some("expected 25 to be 30 // Object.is equality")
    );
    assert!(f.trace.as_deref().unwrap().contains("- Expected"));
    assert_eq!(f.stdout.as_deref(), Some("total is 30"));
    assert_eq!(r.suites[0].name, "src/cart.test.js");
    assert_eq!(r.suites[0].duration_ms, Some(50));
}

#[test]
fn junit_pytest() {
    let r = run(fixture!("pytest-junit.xml"));
    assert_eq!(r.tool.as_deref(), Some("pytest"));
    assert_eq!(r.counts(), counts(2, 1, 1, 1));
    let f = case(&r, "test_rejects_an_empty_cart");
    assert_eq!(f.file.as_deref(), Some("tests/test_orders.py"));
    assert_eq!(f.line, Some(20));
    assert_eq!(f.classname.as_deref(), Some("tests.test_orders"));
    assert_eq!(
        f.message.as_deref(),
        Some("AssertionError: assert 201 == 422")
    );
    assert!(f.trace.as_deref().unwrap().contains("> "));
    let e = case(&r, "test_refunds");
    assert_eq!(e.status, Status::Errored);
    assert!(
        e.message
            .as_deref()
            .unwrap()
            .contains("stripe-mock is not running")
    );
    assert_eq!(e.stderr.as_deref(), Some("WARNING  stripe retrying in 1s"));
    let s = case(&r, "test_sends_a_receipt");
    assert_eq!(s.message.as_deref(), Some("smtp is not configured"));
    assert_eq!(r.started_at.as_deref(), Some("2026-11-01T22:15:30.104Z"));
}

#[test]
fn junit_nested_suites_and_cdata() {
    let r = run(fixture!("surefire-nested.xml"));
    assert_eq!(r.tool.as_deref(), Some("junit"));
    assert_eq!(
        r.suggested_name.as_deref(),
        Some("com.example.shop.AllTests")
    );
    let names: Vec<&str> = r.suites.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        ["com.example.shop.CartTest", "com.example.shop.PriceTest"]
    );
    assert_eq!(r.counts(), counts(3, 0, 0, 1));
    assert_eq!(r.duration_ms, Some(1250));
    assert_eq!(
        case(&r, "removesAnItem").stdout.as_deref(),
        Some(r#"removed <item id="7"/> from the cart"#)
    );
    assert_eq!(
        case(&r, "usesTheLiveRate").message.as_deref(),
        Some("@Disabled: rate service")
    );
}

#[test]
fn junit_is_tolerant() {
    let x = r#"<testsuites><testsuite><testcase/><testcase name="b" time="abc"><failure/></testcase><unknown/></testsuite><testcase name="loose"/></testsuites>"#;
    let r = run(x);
    assert_eq!(r.counts(), counts(2, 1, 0, 0));
    assert_eq!(case(&r, "(no name)").status, Status::Passed);
    assert_eq!(case(&r, "b").duration_ms, None);
    assert_eq!(case(&r, "b").message, None);
    let r = run("<testsuites/>");
    assert_eq!(r.counts(), Counts::default());
    assert_eq!(r.counts().status(), "empty");
}

// ---- XML safety ----

#[test]
fn xml_refuses_doctype_entities_and_external_references() {
    let xxe = r#"<?xml version="1.0"?>
<!DOCTYPE testsuites [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
<testsuites><testsuite name="a"><testcase name="&xxe;"/></testsuite></testsuites>"#;
    assert!(invalid_msg(parse(xxe.as_bytes(), None)).contains("DOCTYPE"));
    assert!(invalid_msg(parse(xxe.as_bytes(), Some(Format::Junit))).contains("DOCTYPE"));
    let laughs = r#"<?xml version="1.0"?>
<!DOCTYPE lolz [
 <!ENTITY lol "lol">
 <!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
 <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
]>
<TestRun>&lol3;</TestRun>"#;
    assert!(invalid_msg(parse(laughs.as_bytes(), Some(Format::Trx))).contains("DOCTYPE"));
    // An empty DOCTYPE, in any case, is refused too.
    let empty = "<!-- x --><!doctype testsuites><testsuites/>";
    assert!(invalid_msg(parse(empty.as_bytes(), Some(Format::Junit))).contains("DOCTYPE"));
    // An entity that no DTD declares is an error.
    let undeclared = r#"<testsuites><testsuite name="&secret;"/></testsuites>"#;
    assert!(invalid_msg(parse(undeclared.as_bytes(), None)).contains("bad XML"));
    // A DOCTYPE inside a CDATA section is text, not a DTD.
    let cdata = "<testsuites><testsuite name=\"s\"><testcase name=\"t\"><system-out><![CDATA[<!DOCTYPE html><p>hi</p>]]></system-out></testcase></testsuite></testsuites>";
    let r = run(cdata);
    assert_eq!(
        case(&r, "t").stdout.as_deref(),
        Some("<!DOCTYPE html><p>hi</p>")
    );
    // The five predefined entities and character references stay.
    let r = run(r#"<testsuite name="s"><testcase name="a &lt; b &amp; &#x63;"/></testsuite>"#);
    assert_eq!(r.suites[0].cases[0].name, "a < b & c");
}

#[test]
fn xml_errors_are_clear() {
    let m = invalid_msg(parse(b"<testsuites><testsuite>", None));
    assert!(m.starts_with("bad XML"), "{m}");
}

// ---- TRX ----

#[test]
fn trx_dotnet_xunit() {
    let r = run(fixture!("dotnet.trx"));
    assert_eq!(r.format, Format::Trx);
    assert_eq!(r.tool.as_deref(), Some("xunit"));
    assert_eq!(r.suggested_name.as_deref(), Some("Shop.Tests"));
    assert_eq!(r.counts(), counts(3, 2, 0, 1));
    assert_eq!(r.suites.len(), 1);
    assert_eq!(r.suites[0].name, "Shop.Tests.CartTests");
    let f = case(&r, "TotalsThePrices");
    assert_eq!(f.classname.as_deref(), Some("Shop.Tests.CartTests"));
    assert_eq!(f.duration_ms, Some(5));
    assert!(
        f.message
            .as_deref()
            .unwrap()
            .starts_with("Assert.Equal() Failure")
    );
    assert!(f.trace.as_deref().unwrap().contains("CartTests.cs:line 9"));
    assert_eq!(f.stdout.as_deref(), Some("total is 25"));
    let s = case(&r, "FetchesLivePrices");
    assert_eq!(s.status, Status::Skipped);
    assert_eq!(s.message.as_deref(), Some("pricing service is down"));
    assert!(
        r.suites[0]
            .cases
            .iter()
            .any(|c| c.name == "AppliesTenPercent(price: 50, expected: 45)")
    );
    assert_eq!(r.started_at.as_deref(), Some("2026-11-01T21:44:02.438Z"));
    assert_eq!(r.duration_ms, Some(1525));
}

#[test]
fn trx_outcomes_durations_and_inner_results() {
    assert_eq!(trx::duration_ms("00:00:01.5000000"), Some(1500));
    assert_eq!(trx::duration_ms("01:02:03"), Some(3_723_000));
    assert_eq!(trx::duration_ms("1.00:00:00.001"), Some(86_400_001));
    assert_eq!(trx::duration_ms("soon"), None);
    let x = r#"<TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">
<Results>
 <UnitTestResult testId="a" testName="N.C.Timeouts" outcome="Timeout" duration="00:01:00"/>
 <UnitTestResult testId="a" testName="N.C.Inconclusive" outcome="Inconclusive"/>
 <UnitTestResult testId="a" testName="N.C.Odd" outcome="Strange"/>
 <UnitTestResult testId="b" testName="Rows" outcome="Failed">
  <InnerResults>
   <UnitTestResult testName="Rows (1)" outcome="Passed"/>
   <UnitTestResult testName="Rows (2)" outcome="Failed"><Output><ErrorInfo><Message>row 2</Message></ErrorInfo></Output></UnitTestResult>
  </InnerResults>
 </UnitTestResult>
</Results>
<TestDefinitions>
 <UnitTest id="a"><TestMethod className="N.C" name="x" adapterTypeName="executor://mstestadapter/v2"/></UnitTest>
 <UnitTest id="b"><TestMethod className="N.D" name="Rows"/></UnitTest>
</TestDefinitions></TestRun>"#;
    let r = run(x);
    assert_eq!(r.tool.as_deref(), Some("mstest"));
    assert_eq!(case(&r, "Timeouts").status, Status::Errored);
    assert_eq!(case(&r, "Timeouts").duration_ms, Some(60_000));
    assert_eq!(case(&r, "Inconclusive").status, Status::Skipped);
    assert_eq!(case(&r, "Odd").status, Status::Errored);
    assert_eq!(case(&r, "Rows (2)").classname.as_deref(), Some("N.D"));
    assert_eq!(case(&r, "Rows (2)").message.as_deref(), Some("row 2"));
    assert_eq!(r.counts(), counts(1, 1, 2, 1));
}

#[test]
fn trx_utf16_with_a_bom() {
    let x =
        "<TestRun><Results><UnitTestResult testName=\"t\" outcome=\"Passed\"/></Results></TestRun>";
    let mut bytes = vec![0xFF, 0xFE];
    for u in x.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let r = parse(&bytes, None).unwrap();
    assert_eq!(r.format, Format::Trx);
    assert_eq!(r.counts(), counts(1, 0, 0, 0));
}

// ---- CTRF ----

#[test]
fn ctrf_playwright() {
    let r = run(fixture!("ctrf.json"));
    assert_eq!(r.tool.as_deref(), Some("playwright"));
    assert_eq!(r.suggested_name.as_deref(), Some("shop-web"));
    assert_eq!(r.counts(), counts(3, 1, 0, 1));
    assert_eq!(r.duration_ms, Some(12_500));
    assert_eq!(r.started_at.as_deref(), Some("2026-11-01T22:00:00.000Z"));
    let f = case(&r, "checkout > shows the receipt");
    assert_eq!(f.file.as_deref(), Some("e2e/checkout.spec.ts"));
    assert_eq!(f.line, Some(31));
    assert_eq!(f.duration_ms, Some(5120));
    assert!(f.message.as_deref().unwrap().contains("toHaveText"));
    assert_eq!(f.stdout.as_deref(), Some("POST /api/pay 502"));
    assert_eq!(r.suites[0].name, "checkout.spec.ts > checkout");
}

#[test]
fn ctrf_statuses_and_suite_lists() {
    let x = r#"{"results":{"tests":[
        {"name":"a","status":"pending","duration":1},
        {"name":"b","status":"other","duration":2,"suite":["e2e","login"]},
        {"name":"c","status":"failed","trace":"Error: boom\n at x"},
        {"status":"passed"},
        "not an object"
    ]}}"#;
    let r = run(x);
    assert_eq!(r.counts(), counts(1, 1, 1, 1));
    assert_eq!(case(&r, "b").status, Status::Errored);
    assert!(r.suites.iter().any(|s| s.name == "e2e > login"));
    assert_eq!(case(&r, "c").message.as_deref(), Some("Error: boom"));
    assert_eq!(r.tool, None);
}

// ---- libtest JSON ----

#[test]
fn libtest_prefixed_events() {
    let r = run(fixture!("prefixed-libtest.json"));
    assert_eq!(r.counts(), counts(2, 0, 0, 0));
    assert_eq!(case(&r, "first").duration_ms, Some(100));
    assert_eq!(
        case(&r, "second").stdout.as_deref(),
        Some("literal { braces }")
    );
    assert!(r.notes.is_empty());
}

#[test]
fn libtest_cargo() {
    let r = run(fixture!("cargo-libtest.json"));
    assert_eq!(r.tool.as_deref(), Some("cargo test"));
    assert_eq!(r.counts(), counts(3, 1, 0, 1));
    assert_eq!(r.duration_ms, Some(2));
    let f = case(&r, "tests::adds_negative_numbers");
    assert_eq!(f.classname.as_deref(), Some("tests"));
    assert_eq!(f.file.as_deref(), Some("src/lib.rs"));
    assert_eq!(f.line, Some(10));
    assert_eq!(
        f.message.as_deref(),
        Some("assertion `left == right` failed: sum of negatives\n  left: -2\n right: -3")
    );
    assert!(f.stdout.as_deref().unwrap().starts_with("checking -1 + -1"));
    let s = case(&r, "tests::adds_many_numbers");
    assert_eq!(s.status, Status::Skipped);
    assert_eq!(s.message.as_deref(), Some("slow on CI"));
    assert_eq!(
        case(&r, "tests::division::divides").classname.as_deref(),
        Some("tests::division")
    );
}

#[test]
fn libtest_nextest_binaries_crashes_and_junk_lines() {
    let x = concat!(
        "   Compiling calc v0.1.0\n",
        r#"{"type":"suite","event":"started","test_count":2,"nextest":{"crate":"calc","test_binary":"calc","kind":"lib"}}"#,
        "\n",
        r#"{"type":"test","event":"started","name":"calc$tests::a"}"#,
        "\n",
        r#"{"type":"test","event":"ok","name":"calc$tests::a","exec_time":0.5}"#,
        "\n",
        r#"{"type":"test","event":"started","name":"calc::it$crashes"}"#,
        "\n",
        r#"{"type":"suite","event":"started","test_count":1}"#,
        "\n",
        r#"{"type":"test","event":"failed","name":"old","stdout":"thread 'old' panicked at 'boom', src/x.rs:3:5\n"}"#,
        "\n",
    );
    let r = run(x);
    assert_eq!(r.tool.as_deref(), Some("cargo-nextest"));
    let names: Vec<&str> = r.suites.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["calc", "calc::it", "test binary 2"]);
    assert_eq!(case(&r, "tests::a").duration_ms, Some(500));
    let crash = case(&r, "crashes");
    assert_eq!(crash.status, Status::Errored);
    assert_eq!(
        crash.message.as_deref(),
        Some("incomplete: no end event in the output")
    );
    let old = case(&r, "old");
    assert_eq!(old.message.as_deref(), Some("boom"));
    assert_eq!(old.file.as_deref(), Some("src/x.rs"));
    assert_eq!(old.line, Some(3));
    assert_eq!(r.notes, ["1 lines are not JSON; they were skipped"]);
}

// ---- Vitest and Jest ----

#[test]
fn vitest_json() {
    let r = run(fixture!("vitest.json"));
    assert_eq!(r.tool.as_deref(), Some("vitest"));
    assert_eq!(r.counts(), counts(3, 2, 0, 2));
    let names: Vec<&str> = r.suites.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["src/cart.test.js", "src/format.test.js"]);
    let f = case(&r, "cart > totals the prices");
    assert_eq!(f.classname.as_deref(), Some("cart"));
    assert_eq!(f.file.as_deref(), Some("src/cart.test.js"));
    assert_eq!(f.duration_ms, Some(28));
    assert_eq!(
        f.message.as_deref(),
        Some("AssertionError: expected 25 to be 30 // Object.is equality")
    );
    assert_eq!(
        case(&r, "cart > discounts > expires old codes").status,
        Status::Skipped
    );
    assert!(r.started_at.is_some());
    assert!(r.duration_ms.is_some());
}

#[test]
fn jest_json() {
    let r = run(fixture!("jest.json"));
    assert_eq!(r.tool.as_deref(), Some("jest"));
    assert_eq!(r.counts(), counts(3, 1, 0, 2));
    assert_eq!(r.duration_ms, Some(1250));
    let f = case(
        &r,
        "login form > validation > shows an error for an empty email",
    );
    assert_eq!(f.line, Some(24));
    assert_eq!(f.file.as_deref(), Some("src/login.test.tsx"));
    assert_eq!(f.classname.as_deref(), Some("login form > validation"));
    // ANSI codes stay in the text; the dashboard takes them out.
    assert!(f.trace.as_deref().unwrap().contains('\u{1b}'));
    assert_eq!(case(&r, "formats a price").classname, None);
}

#[test]
fn jest_a_file_that_did_not_run() {
    let x = r#"{"testResults":[{"name":"/w/src/a.test.ts","status":"failed","message":"SyntaxError: Unexpected token\n at a.test.ts:3","assertionResults":[]},{"name":"/w/src/b.test.ts","status":"passed","assertionResults":[{"title":"t","status":"passed"}]}]}"#;
    let r = run(x);
    assert_eq!(r.format, Format::JestJson);
    let e = case(&r, "(the file did not run)");
    assert_eq!(e.status, Status::Errored);
    assert_eq!(e.message.as_deref(), Some("SyntaxError: Unexpected token"));
    assert_eq!(r.suites[0].name, "src/a.test.ts");
}

// ---- TAP ----

#[test]
fn tap_node_subtests() {
    let r = run(fixture!("node.tap"));
    assert_eq!(r.counts(), counts(2, 1, 0, 2));
    let names: Vec<&str> = r.suites.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["cart", "tests"]);
    let f = case(&r, "totals the prices");
    assert_eq!(f.classname.as_deref(), Some("cart"));
    assert_eq!(f.duration_ms, Some(6));
    assert_eq!(f.line, Some(5));
    assert!(f.file.as_deref().unwrap().ends_with("cart.test.mjs"));
    assert!(
        f.message
            .as_deref()
            .unwrap()
            .starts_with("Expected values to be strictly equal")
    );
    assert!(f.trace.as_deref().unwrap().contains("Test.runInAsyncScope"));
    let s = case(&r, "stacks two codes");
    assert_eq!(s.status, Status::Skipped);
    assert_eq!(s.message.as_deref(), Some("SKIP: codes service is down"));
    assert_eq!(
        case(&r, "expires old codes").message.as_deref(),
        Some("TODO")
    );
    assert_eq!(case(&r, "formats cents").status, Status::Passed);
    assert!(r.notes.is_empty(), "{:?}", r.notes);
}

#[test]
fn tap_basic_yaml_and_todo() {
    let r = run(fixture!("basic.tap"));
    assert_eq!(r.counts(), counts(2, 1, 0, 2));
    let f = case(&r, "rejects a bad port");
    assert_eq!(f.message.as_deref(), Some("port 70000 is out of range"));
    assert!(f.trace.as_deref().unwrap().contains("severity: fail"));
    // A TODO that fails does not fail the run.
    let t = case(&r, "merges two files");
    assert_eq!(t.status, Status::Skipped);
    assert_eq!(t.message.as_deref(), Some("TODO: merge is not written yet"));
}

#[test]
fn tap_bail_out_and_plan() {
    let r = run("TAP version 13\n1..3\nok 1 - a\nnot ok 2\nBail out! database is gone\n");
    assert_eq!(r.counts(), counts(1, 1, 1, 0));
    assert_eq!(case(&r, "test 2").status, Status::Failed);
    assert_eq!(
        case(&r, "Bail out!").message.as_deref(),
        Some("database is gone")
    );
    assert!(r.notes.iter().any(|n| n.contains("plan says 3")));
    assert!(r.notes.iter().any(|n| n.contains("bailed out")));
}

// ---- limits ----

#[test]
fn size_limit() {
    let big = vec![b' '; MAX_BYTES + 1];
    assert_eq!(
        parse(&big, None),
        Err(ParseError::TooLarge {
            size: MAX_BYTES + 1,
            max: MAX_BYTES
        })
    );
}

#[test]
fn case_limit() {
    let mut x = String::from("1..5\n");
    for i in 0..5 {
        x.push_str(&format!("ok {i}\n"));
    }
    let m = invalid_msg(parse_with(
        x.as_bytes(),
        None,
        Sink::with_limits(4, MAX_TEXT),
    ));
    assert!(m.contains("more than 4 test cases"), "{m}");
    assert!(parse_with(x.as_bytes(), None, Sink::with_limits(5, MAX_TEXT)).is_ok());
}

#[test]
fn text_is_truncated_with_a_note() {
    let long = "é".repeat(50);
    let x = format!(
        r#"{{"results":{{"tests":[{{"name":"a","status":"failed","message":"{long}","trace":"short"}}]}}}}"#
    );
    let r = parse_with(x.as_bytes(), None, Sink::with_limits(10, 11)).unwrap();
    let m = r.suites[0].cases[0].message.clone().unwrap();
    assert!(m.starts_with("ééééé\n[truncated: 90 bytes more]"), "{m}");
    assert_eq!(r.suites[0].cases[0].trace.as_deref(), Some("short"));
    assert_eq!(r.notes, ["1 texts were cut to 0 KiB"]);
}

#[test]
fn a_real_size_run_parses() {
    let mut x = String::from("<testsuites>");
    for s in 0..50 {
        x.push_str(&format!("<testsuite name=\"s{s}\" time=\"1\">"));
        for c in 0..200 {
            if c % 50 == 0 {
                x.push_str(&format!(
                    "<testcase name=\"c{c}\"><failure message=\"m\">{}</failure></testcase>",
                    "x".repeat(70_000)
                ));
            } else {
                x.push_str(&format!("<testcase name=\"c{c}\" time=\"0.001\"/>"));
            }
        }
        x.push_str("</testsuite>");
    }
    x.push_str("</testsuites>");
    let r = run(&x);
    assert_eq!(r.case_count(), 10_000);
    assert_eq!(r.counts().failed, 200);
    assert_eq!(r.duration_ms, Some(50_000));
    assert!(r.notes[0].starts_with("200 texts were cut to 64 KiB"));
}

#[test]
fn counts_status() {
    assert_eq!(counts(1, 0, 0, 3).status(), "passed");
    assert_eq!(counts(1, 0, 1, 0).status(), "failed");
    assert_eq!(counts(0, 0, 0, 0).status(), "empty");
    assert_eq!(counts(0, 0, 0, 2).status(), "passed");
}
