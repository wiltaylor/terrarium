#!/usr/bin/env bash
# Upload example test runs to a project (PLAN §15.5, docs/api.md §29), one
# or more in each format: JUnit XML (cargo-nextest), TRX (.NET), CTRF
# (Playwright), libtest JSON (cargo test), Vitest JSON, Jest JSON, and TAP
# (node --test). Some runs pass and some fail. The two runs named `unit`
# show the compare: the second run has a newly failing and a fixed test.
#
# Usage: scripts/seed-test-runs.sh [project]        (default project: demo)
#
# The script uses only the `terrarium` CLI. By default it runs the CLI in
# the Docker stack: `docker compose exec -T terrarium terrarium`, and it
# copies each file into the container (/tmp/seed-test-runs/) first. Set
# `T` to use another command, for example `T=terrarium` for a local
# daemon. Then the CLI reads the files from a local temp dir.
#
# The script is idempotent: each run has a name and a fixed `--commit`
# marker, and the script skips a run when the project has a run with the
# same name and commit. It links the runs to the first two cards of the
# board, and makes two demo cards when the board has fewer. It links the
# second `unit` run to the first version in `draft` or `rejected`, when
# there is one. It makes the project when it is missing. It needs python3
# to read the JSON answers.

set -euo pipefail

P="${1:-demo}"
DEFAULT_T="docker compose exec -T terrarium terrarium"
T="${T:-$DEFAULT_T}"
read -r -a TCMD <<<"$T"
# In the Docker stack the CLI reads the files in the container.
IN_DOCKER=0
[ "$T" = "$DEFAULT_T" ] && IN_DOCKER=1

# `docker compose` needs the dir of compose.yaml.
cd "$(dirname "$0")/.."

t() { "${TCMD[@]}" -p "$P" "$@"; }
tj() { "${TCMD[@]}" -p "$P" --json "$@"; }
quiet() { "$@" >/dev/null 2>&1; }

# json <python expression of `d`>: read JSON on stdin, print the result.
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
REMOTE_DIR=/tmp/seed-test-runs

if ! quiet "${TCMD[@]}" project show "$P"; then
  echo "making project $P"
  "${TCMD[@]}" project new "$P" -d "Demo project" --prefix DEMO >/dev/null
fi

# ---- cards and a version to link to ----

CARDS=()
while IFS= read -r id; do
  [ -n "$id" ] && CARDS+=("$id")
done < <(tj card ls | json "'\n'.join(c['id'] for c in (d if isinstance(d, list) else d.get('cards', []))[:2])")
while [ "${#CARDS[@]}" -lt 2 ]; do
  n=$((${#CARDS[@]} + 1))
  id=$(tj card new "Demo: test results $n" -b "A demo card for the example test runs." | json "d['id']")
  echo "made card $id"
  CARDS+=("$id")
done
CARD1=${CARDS[0]}
CARD2=${CARDS[1]}

VERSION=$(tj version ls | json "next((v['name'] for v in d if v.get('status') in ('draft', 'rejected')), '')")

# ---- push ----

# push <file> <name> <commit> [args...]: upload the file, unless the
# project has a run with this name and commit.
push() {
  local file=$1 name=$2 commit=$3
  shift 3
  local have
  have=$(tj test ls --all --name "$name" | json "sum(1 for r in d if r.get('commit') == '$commit')")
  if [ "$have" != "0" ]; then
    echo "run $name ($commit) exists: skip it"
    return
  fi
  local path="$WORK/$file"
  if [ "$IN_DOCKER" = 1 ]; then
    docker compose exec -T terrarium sh -c "mkdir -p $REMOTE_DIR && cat > $REMOTE_DIR/$file" <"$WORK/$file"
    path="$REMOTE_DIR/$file"
  fi
  t test push "$path" --name "$name" --commit "$commit" --branch main "$@"
}

# ---- the files ----

# unit, run 1 (cargo-nextest JUnit): 1 of 6 fails.
cat >"$WORK/unit-1.xml" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="6" failures="1" errors="0" uuid="5b1f9c1e-2c0a-4d6e-9a43-0f3b8e2d7a10" timestamp="2026-11-01T09:12:03.512+00:00" time="2.418">
    <testsuite name="terrariumd" tests="4" disabled="0" errors="0" failures="1">
        <testcase name="store::test_runs::tests::creates_a_run" classname="terrariumd" timestamp="2026-11-01T09:12:03.520+00:00" time="0.041">
        </testcase>
        <testcase name="store::test_runs::tests::lists_newest_first" classname="terrariumd" timestamp="2026-11-01T09:12:03.522+00:00" time="0.037">
        </testcase>
        <testcase name="store::test_runs::tests::compares_with_previous" classname="terrariumd" timestamp="2026-11-01T09:12:03.524+00:00" time="0.052">
            <failure type="test failure with exit code 101">thread 'store::test_runs::tests::compares_with_previous' panicked at crates/terrariumd/src/store/test_runs.rs:812:9:
assertion `left == right` failed
  left: 0
 right: 1
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace</failure>
            <system-out>
running 1 test
test store::test_runs::tests::compares_with_previous ... FAILED
</system-out>
            <system-err>thread 'store::test_runs::tests::compares_with_previous' panicked at crates/terrariumd/src/store/test_runs.rs:812:9:
assertion `left == right` failed
  left: 0
 right: 1
</system-err>
        </testcase>
        <testcase name="test_results::junit::tests::nested_suites" classname="terrariumd" timestamp="2026-11-01T09:12:03.530+00:00" time="0.008">
        </testcase>
    </testsuite>
    <testsuite name="terrarium" tests="2" disabled="0" errors="0" failures="0">
        <testcase name="commands::tests::test_push_prints_counts" classname="terrarium" timestamp="2026-11-01T09:12:03.540+00:00" time="0.112">
        </testcase>
        <testcase name="commands::tests::test_ls_filters" classname="terrarium" timestamp="2026-11-01T09:12:03.541+00:00" time="0.098">
        </testcase>
    </testsuite>
</testsuites>
EOF

# unit, run 2: compares_with_previous is fixed, nested_suites fails now,
# and a new test is there.
cat >"$WORK/unit-2.xml" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="7" failures="1" errors="0" uuid="8c4d2a77-91e3-4b0f-b6a5-3e1d0c9f4b22" timestamp="2026-11-02T08:40:11.204+00:00" time="2.733">
    <testsuite name="terrariumd" tests="5" disabled="0" errors="0" failures="1">
        <testcase name="store::test_runs::tests::creates_a_run" classname="terrariumd" timestamp="2026-11-02T08:40:11.210+00:00" time="0.039">
        </testcase>
        <testcase name="store::test_runs::tests::lists_newest_first" classname="terrariumd" timestamp="2026-11-02T08:40:11.212+00:00" time="0.035">
        </testcase>
        <testcase name="store::test_runs::tests::compares_with_previous" classname="terrariumd" timestamp="2026-11-02T08:40:11.214+00:00" time="0.049">
        </testcase>
        <testcase name="test_results::junit::tests::nested_suites" classname="terrariumd" timestamp="2026-11-02T08:40:11.220+00:00" time="0.011">
            <failure type="test failure with exit code 101">thread 'test_results::junit::tests::nested_suites' panicked at crates/terrariumd/src/test_results/junit.rs:388:9:
expected 2 suites, got 1
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace</failure>
            <system-err>thread 'test_results::junit::tests::nested_suites' panicked at crates/terrariumd/src/test_results/junit.rs:388:9:
expected 2 suites, got 1
</system-err>
        </testcase>
        <testcase name="test_results::junit::tests::rejects_doctype" classname="terrariumd" timestamp="2026-11-02T08:40:11.224+00:00" time="0.004">
        </testcase>
    </testsuite>
    <testsuite name="terrarium" tests="2" disabled="0" errors="0" failures="0">
        <testcase name="commands::tests::test_push_prints_counts" classname="terrarium" timestamp="2026-11-02T08:40:11.240+00:00" time="0.104">
        </testcase>
        <testcase name="commands::tests::test_ls_filters" classname="terrarium" timestamp="2026-11-02T08:40:11.241+00:00" time="0.091">
        </testcase>
    </testsuite>
</testsuites>
EOF

# dotnet (TRX): passes, with one test not executed.
cat >"$WORK/dotnet.trx" <<'EOF'
<?xml version="1.0" encoding="utf-8"?>
<TestRun id="3f2a9d1c-7b44-4e0a-a1c2-5d6e7f809a1b" name="builder@ci 2026-11-02 08:15:42" runUser="builder" xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">
  <Times creation="2026-11-02T08:15:42.1180000+00:00" queuing="2026-11-02T08:15:42.1180000+00:00" start="2026-11-02T08:15:40.9010000+00:00" finish="2026-11-02T08:15:42.3440000+00:00" />
  <TestSettings name="default" id="0e3f7c55-4a7b-4f43-9a3d-2b1e4c6d8f90">
    <Deployment runDeploymentRoot="builder_ci_2026-11-02_08_15_42" />
  </TestSettings>
  <Results>
    <UnitTestResult executionId="a1f0c3d2-0001-4c1e-8b2a-000000000001" testId="7d1e2f30-0001-4a5b-9c6d-000000000001" testName="Invoices.Tests.TotalsTests.Sum_of_lines" computerName="ci" duration="00:00:00.0123456" startTime="2026-11-02T08:15:41.1000000+00:00" endTime="2026-11-02T08:15:41.1130000+00:00" testType="13cdc9d9-ddb5-4fa4-a97d-d965ccfc6d4b" outcome="Passed" testListId="8c84fa94-04c1-424b-9868-57a2d4851a1d" relativeResultsDirectory="a1f0c3d2-0001-4c1e-8b2a-000000000001" />
    <UnitTestResult executionId="a1f0c3d2-0002-4c1e-8b2a-000000000002" testId="7d1e2f30-0002-4a5b-9c6d-000000000002" testName="Invoices.Tests.TotalsTests.Tax_is_rounded" computerName="ci" duration="00:00:00.0045120" startTime="2026-11-02T08:15:41.1200000+00:00" endTime="2026-11-02T08:15:41.1250000+00:00" testType="13cdc9d9-ddb5-4fa4-a97d-d965ccfc6d4b" outcome="Passed" testListId="8c84fa94-04c1-424b-9868-57a2d4851a1d" relativeResultsDirectory="a1f0c3d2-0002-4c1e-8b2a-000000000002">
      <Output>
        <StdOut>Rounded 10.005 to 10.01</StdOut>
      </Output>
    </UnitTestResult>
    <UnitTestResult executionId="a1f0c3d2-0003-4c1e-8b2a-000000000003" testId="7d1e2f30-0003-4a5b-9c6d-000000000003" testName="Invoices.Tests.ExportTests.Pdf_export" computerName="ci" duration="00:00:00.0000010" startTime="2026-11-02T08:15:41.1300000+00:00" endTime="2026-11-02T08:15:41.1300000+00:00" testType="13cdc9d9-ddb5-4fa4-a97d-d965ccfc6d4b" outcome="NotExecuted" testListId="8c84fa94-04c1-424b-9868-57a2d4851a1d" relativeResultsDirectory="a1f0c3d2-0003-4c1e-8b2a-000000000003">
      <Output>
        <ErrorInfo>
          <Message>Skipped: needs a PDF renderer</Message>
        </ErrorInfo>
      </Output>
    </UnitTestResult>
  </Results>
  <TestDefinitions>
    <UnitTest name="Invoices.Tests.TotalsTests.Sum_of_lines" storage="/src/tests/invoices.tests/bin/debug/net8.0/invoices.tests.dll" id="7d1e2f30-0001-4a5b-9c6d-000000000001">
      <Execution id="a1f0c3d2-0001-4c1e-8b2a-000000000001" />
      <TestMethod codeBase="/src/tests/Invoices.Tests/bin/Debug/net8.0/Invoices.Tests.dll" adapterTypeName="executor://xunit/VsTestRunner2/netcoreapp" className="Invoices.Tests.TotalsTests" name="Sum_of_lines" />
    </UnitTest>
    <UnitTest name="Invoices.Tests.TotalsTests.Tax_is_rounded" storage="/src/tests/invoices.tests/bin/debug/net8.0/invoices.tests.dll" id="7d1e2f30-0002-4a5b-9c6d-000000000002">
      <Execution id="a1f0c3d2-0002-4c1e-8b2a-000000000002" />
      <TestMethod codeBase="/src/tests/Invoices.Tests/bin/Debug/net8.0/Invoices.Tests.dll" adapterTypeName="executor://xunit/VsTestRunner2/netcoreapp" className="Invoices.Tests.TotalsTests" name="Tax_is_rounded" />
    </UnitTest>
    <UnitTest name="Invoices.Tests.ExportTests.Pdf_export" storage="/src/tests/invoices.tests/bin/debug/net8.0/invoices.tests.dll" id="7d1e2f30-0003-4a5b-9c6d-000000000003">
      <Execution id="a1f0c3d2-0003-4c1e-8b2a-000000000003" />
      <TestMethod codeBase="/src/tests/Invoices.Tests/bin/Debug/net8.0/Invoices.Tests.dll" adapterTypeName="executor://xunit/VsTestRunner2/netcoreapp" className="Invoices.Tests.ExportTests" name="Pdf_export" />
    </UnitTest>
  </TestDefinitions>
  <TestEntries>
    <TestEntry testId="7d1e2f30-0001-4a5b-9c6d-000000000001" executionId="a1f0c3d2-0001-4c1e-8b2a-000000000001" testListId="8c84fa94-04c1-424b-9868-57a2d4851a1d" />
    <TestEntry testId="7d1e2f30-0002-4a5b-9c6d-000000000002" executionId="a1f0c3d2-0002-4c1e-8b2a-000000000002" testListId="8c84fa94-04c1-424b-9868-57a2d4851a1d" />
    <TestEntry testId="7d1e2f30-0003-4a5b-9c6d-000000000003" executionId="a1f0c3d2-0003-4c1e-8b2a-000000000003" testListId="8c84fa94-04c1-424b-9868-57a2d4851a1d" />
  </TestEntries>
  <TestLists>
    <TestList name="Results Not in a List" id="8c84fa94-04c1-424b-9868-57a2d4851a1d" />
    <TestList name="All Loaded Results" id="19431567-8539-422a-85d7-44ee4e166bda" />
  </TestLists>
  <ResultSummary outcome="Completed">
    <Counters total="3" executed="2" passed="2" failed="0" error="0" timeout="0" aborted="0" inconclusive="0" passedButRunAborted="0" notRunnable="0" notExecuted="1" disconnected="0" warning="0" completed="0" inProgress="0" pending="0" />
  </ResultSummary>
</TestRun>
EOF

# e2e (Playwright CTRF): 1 fails, 1 is skipped.
cat >"$WORK/e2e-ctrf.json" <<'EOF'
{
  "results": {
    "tool": { "name": "playwright", "version": "1.48.2" },
    "summary": {
      "tests": 4, "passed": 2, "failed": 1, "skipped": 1, "pending": 0, "other": 0,
      "start": 1730535600000, "stop": 1730535618450
    },
    "tests": [
      { "name": "login > signs in with a password", "status": "passed", "duration": 2210,
        "suite": "auth.spec.ts > login", "filePath": "tests/auth.spec.ts", "browser": "chromium" },
      { "name": "login > shows an error for a bad password", "status": "passed", "duration": 1874,
        "suite": "auth.spec.ts > login", "filePath": "tests/auth.spec.ts", "browser": "chromium" },
      { "name": "board > drags a card to review", "status": "failed", "duration": 12043,
        "suite": "board.spec.ts > board", "filePath": "tests/board.spec.ts", "browser": "chromium",
        "message": "\u001b[31mError: Timed out 10000ms waiting for expect(locator).toBeVisible()\u001b[39m\n\nLocator: getByRole('article', { name: 'DEMO-2' })\nExpected: visible\nReceived: <element(s) not found>",
        "trace": "    at /work/web/tests/board.spec.ts:41:58\n    at /work/web/node_modules/@playwright/test/lib/worker/workerMain.js:327:7",
        "rawStatus": "failed", "retries": 1, "flaky": false },
      { "name": "board > prints the board", "status": "skipped", "duration": 0,
        "suite": "board.spec.ts > board", "filePath": "tests/board.spec.ts", "browser": "chromium" }
    ]
  }
}
EOF

# cargo-test (libtest JSON): all pass.
cat >"$WORK/cargo-test.json" <<'EOF'
{ "type": "suite", "event": "started", "test_count": 4 }
{ "type": "test", "event": "started", "name": "eta::tests::parses_hours" }
{ "type": "test", "event": "started", "name": "eta::tests::rejects_more_than_30_days" }
{ "type": "test", "event": "started", "name": "limits::tests::truncates_long_text" }
{ "type": "test", "event": "started", "name": "limits::tests::keeps_short_text" }
{ "type": "test", "name": "eta::tests::parses_hours", "event": "ok", "exec_time": 0.000412 }
{ "type": "test", "name": "eta::tests::rejects_more_than_30_days", "event": "ok", "exec_time": 0.000388 }
{ "type": "test", "name": "limits::tests::keeps_short_text", "event": "ok", "exec_time": 0.000121 }
{ "type": "test", "name": "limits::tests::truncates_long_text", "event": "ok", "exec_time": 0.001903 }
{ "type": "suite", "event": "ok", "passed": 4, "failed": 0, "ignored": 0, "measured": 0, "filtered_out": 0, "exec_time": 0.004817 }
EOF

# web-unit (Vitest JSON): all pass.
cat >"$WORK/vitest.json" <<'EOF'
{"numTotalTestSuites":2,"numPassedTestSuites":2,"numFailedTestSuites":0,"numPendingTestSuites":0,"numTotalTests":3,"numPassedTests":3,"numFailedTests":0,"numPendingTests":0,"numTodoTests":0,"snapshot":{"added":0,"failure":false,"filesAdded":0,"filesRemoved":0,"filesRemovedList":[],"filesUnmatched":0,"filesUpdated":0,"matched":0,"total":0,"unchecked":0,"uncheckedKeysByFile":[],"unmatched":0,"updated":0,"didUpdate":false},"startTime":1730536800123,"success":true,"testResults":[{"assertionResults":[{"ancestorTitles":["countsBar"],"fullName":"countsBar splits the bar by status","status":"passed","title":"splits the bar by status","duration":3,"failureMessages":[],"meta":{}},{"ancestorTitles":["countsBar"],"fullName":"countsBar has no bar for an empty run","status":"passed","title":"has no bar for an empty run","duration":1,"failureMessages":[],"meta":{}}],"startTime":1730536800301,"endTime":1730536800305,"status":"passed","message":"","name":"/work/web/src/features/tests/model.test.ts"},{"assertionResults":[{"ancestorTitles":["stripAnsi"],"fullName":"stripAnsi removes colour codes","status":"passed","title":"removes colour codes","duration":1,"failureMessages":[],"meta":{}}],"startTime":1730536800310,"endTime":1730536800311,"status":"passed","message":"","name":"/work/web/src/lib/ansi.test.ts"}]}
EOF

# api (Jest JSON): 1 fails.
cat >"$WORK/jest.json" <<'EOF'
{"numFailedTestSuites":1,"numFailedTests":1,"numPassedTestSuites":0,"numPassedTests":2,"numPendingTestSuites":0,"numPendingTests":0,"numRuntimeErrorTestSuites":0,"numTodoTests":0,"numTotalTestSuites":1,"numTotalTests":3,"openHandles":[],"snapshot":{"added":0,"didUpdate":false,"failure":false,"filesAdded":0,"filesRemoved":0,"filesRemovedList":[],"filesUnmatched":0,"filesUpdated":0,"matched":0,"total":0,"unchecked":0,"uncheckedKeysByFile":[],"unmatched":0,"updated":0},"startTime":1730537400000,"success":false,"testResults":[{"assertionResults":[{"ancestorTitles":["orders API","POST /orders"],"duration":18,"failureDetails":[],"failureMessages":[],"fullName":"orders API POST /orders creates an order","invocations":1,"location":null,"numPassingAsserts":2,"retryReasons":[],"status":"passed","title":"creates an order"},{"ancestorTitles":["orders API","POST /orders"],"duration":7,"failureDetails":[],"failureMessages":[],"fullName":"orders API POST /orders rejects an empty cart","invocations":1,"location":null,"numPassingAsserts":1,"retryReasons":[],"status":"passed","title":"rejects an empty cart"},{"ancestorTitles":["orders API","GET /orders/:id"],"duration":11,"failureDetails":[{}],"failureMessages":["Error: \u001b[2mexpect(\u001b[22m\u001b[31mreceived\u001b[39m\u001b[2m).\u001b[22mtoBe\u001b[2m(\u001b[22m\u001b[32mexpected\u001b[39m\u001b[2m) // Object.is equality\u001b[22m\n\nExpected: \u001b[32m404\u001b[39m\nReceived: \u001b[31m500\u001b[39m\n    at Object.<anonymous> (/work/api/test/orders.test.js:37:24)"],"fullName":"orders API GET /orders/:id answers 404 for a missing order","invocations":1,"location":null,"numPassingAsserts":0,"retryReasons":[],"status":"failed","title":"answers 404 for a missing order"}],"endTime":1730537401250,"message":"","name":"/work/api/test/orders.test.js","startTime":1730537400100,"status":"failed","summary":""}],"wasInterrupted":false}
EOF

# node (TAP 13 from node --test): passes, with a skip and a todo.
cat >"$WORK/node.tap" <<'EOF'
TAP version 13
# Subtest: parses a cron line
ok 1 - parses a cron line
  ---
  duration_ms: 1.204
  ...
# Subtest: rejects a bad day
ok 2 - rejects a bad day
  ---
  duration_ms: 0.388
  ...
# Subtest: handles a leap year
ok 3 - handles a leap year # SKIP needs the tz database
  ---
  duration_ms: 0.051
  ...
# Subtest: handles daylight saving
not ok 4 - handles daylight saving # TODO not written yet
  ---
  duration_ms: 0.044
  ...
1..4
# tests 4
# pass 2
# fail 0
# skipped 1
# todo 1
# duration_ms 41.73
EOF

# ---- upload ----

push unit-1.xml unit 4f1c2a9 --card "$CARD1"
if [ -n "$VERSION" ]; then
  push unit-2.xml unit 7b3e8d0 --card "$CARD1" --version "$VERSION"
else
  push unit-2.xml unit 7b3e8d0 --card "$CARD1"
fi
push dotnet.trx dotnet 7b3e8d0 --card "$CARD2"
push e2e-ctrf.json e2e 7b3e8d0 --card "$CARD2"
push cargo-test.json cargo-test 7b3e8d0
push vitest.json web-unit 7b3e8d0 --card "$CARD2"
push jest.json api 7b3e8d0
push node.tap node 7b3e8d0

echo "done: open /p/$P/board/tests"
