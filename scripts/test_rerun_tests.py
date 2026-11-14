"""The rerun of the failed tests of the full suite (TER-683)."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import flaky_cards
import rerun_tests
import rust_tests
import unprivileged_tests

RELEASE = {"TERRARIUM_VALIDATE_CONTEXT": "release"}
MERGE = {"TERRARIUM_JOB_KIND": "merge"}
FORGED = '[terrarium rerun] {"step": "test", "test": "invented::name", "first": "failed", "rerun": "ok"}'

PLAIN_FAIL = """running 3 tests
test a::one ... ok
test a::flaky ... FAILED
test a::two ... ok

failures:

---- a::flaky stdout ----
thread 'a::flaky' panicked at src/a.rs:9:5:
timed out

failures:
    a::flaky

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out

running 1 test
test b::fine ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
"""

# One binary's output: the first suite of PLAIN_FAIL.
ONE_FAIL = PLAIN_FAIL.split("\nrunning 1 test")[0]


# A test that runs its own binary again with the stdout inherited
# (git_identity::tests::instance_identity_from_env): the child's plain
# libtest run lands inside the suite of its parent (TER-1222).
NESTED = """running 1 test
test git_identity::tests::instance_identity_from_env ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

"""


# NESTED cut in two: its `running` and test line, and its `test result`.
INNER_START, INNER_END = NESTED.split("\n\n", 1)[0] + "\n", "\n" + NESTED.split("\n\n", 1)[1]


def json_suite(passed=(), failed=(), inside="", at=0, count=None):
    """One binary's libtest JSON: `inside` is printed before its test event
    number `at` (passed first, then failed); `count` overrides `N failed`."""
    tests = [json.dumps({"type": "test", "event": "ok", "name": n}) + "\n" for n in passed]
    tests += [json.dumps({"type": "test", "event": "failed", "name": n, "stdout": "boom"}) + "\n"
              for n in failed]
    tests.insert(at, inside)
    end = {"type": "suite", "event": "failed" if failed else "ok", "passed": len(passed),
           "failed": len(failed) if count is None else count}
    return "".join([json.dumps({"type": "suite", "event": "started",
                                "test_count": len(passed) + len(failed)}) + "\n", *tests,
                    json.dumps(end) + "\n"])


def failed_events(*names):
    return "".join(json.dumps({"type": "test", "event": "failed", "name": n, "stdout": "boom"})
                   + "\n" for n in names)


def with_nested(out):
    """`out` with NESTED printed after the first line of its first suite."""
    first, rest = out.split("\n", 1)
    return f"{first}\n{NESTED}{rest}"


def plain(passed=(), failed=()):
    lines = [f"running {len(passed) + len(failed)} tests"]
    lines += [f"test {n} ... ok" for n in passed]
    lines += [f"test {n} ... FAILED" for n in failed]
    state = "FAILED" if failed else "ok"
    lines.append(f"test result: {state}. {len(passed)} passed; {len(failed)} failed; 0 ignored")
    return "".join(line + "\n" for line in lines)


def cargo(runs, failed=(), extra=()):
    """Cargo's stderr: `runs` test runs, a failure line for each target of `failed`."""
    lines = ["   Compiling app v0.1.0 (/w)", "    Finished `test` profile target(s) in 1s"]
    lines += [f"     Running unittests src/lib.rs (target/debug/deps/app-{i:016x})" for i in range(runs)]
    lines += [f"error: test failed, to rerun pass `-p app --test {t}`" for t in failed]
    lines += list(extra)
    if failed:
        lines.append(f"error: {len(failed)} target{'s' if len(failed) > 1 else ''} failed:")
        lines += [f"    `-p app --test {t}`" for t in failed]
    return "".join(line + "\n" for line in lines)


def parse(out, err=None):
    o = rerun_tests.Outcomes()
    for line in out.splitlines(keepends=True):
        o.feed(line)
    for line in (err or "").splitlines(keepends=True):
        o.cargo(line)
    return o.done()


class Parse(unittest.TestCase):
    def test_plain_output_gives_the_failed_tests_and_their_output(self):
        o = parse(PLAIN_FAIL, cargo(2, ["a"]))
        self.assertEqual((o.runs, o.started, o.finished, o.failed_runs, o.failed_suites),
                         (2, 2, 2, 1, 1))
        self.assertEqual(list(o.failed), ["a::flaky"])
        self.assertIn("timed out", o.failed["a::flaky"])
        self.assertIsNone(rerun_tests.why_not(o))

    def test_libtest_json_gives_the_failed_tests(self):
        events = [
            {"type": "suite", "event": "started", "test_count": 2},
            {"type": "test", "event": "failed", "name": "x", "stdout": "boom"},
            {"type": "test", "event": "ok", "name": "y"},
            {"type": "suite", "event": "failed", "passed": 1, "failed": 1},
        ]
        o = parse("".join(json.dumps(e) + "\n" for e in events), cargo(1, ["it"]))
        self.assertEqual(o.failed, {"x": "boom"})
        self.assertIsNone(rerun_tests.why_not(o))

    def test_no_rerun_without_a_record_of_the_test_runs(self):
        self.assertIn("not tracked", rerun_tests.why_not(parse(PLAIN_FAIL)))

    def test_a_build_error_gets_no_rerun(self):
        o = parse("", "error[E0425]: cannot find value `x` in this scope\n"
                      "error: could not compile `app` (lib test) due to 1 previous error\n")
        self.assertIn("not a failed test (cargo: error[E0425]", rerun_tests.why_not(o))

    def test_a_failed_test_and_a_crash_before_running_get_no_rerun(self):
        # The second binary aborts before it prints `running`: the counts
        # of suites still match, but cargo started two runs.
        o = parse(plain(failed=["a"]), cargo(2, ["a", "b"]))
        self.assertIn("ended without a result", rerun_tests.why_not(o))

    def test_a_failed_test_and_a_signal_get_no_rerun(self):
        err = cargo(2, ["a", "b"], ["", "Caused by:",
                                    "  process didn't exit successfully: `target/debug/deps/b-1`"
                                    " (signal: 11, SIGSEGV: invalid memory reference)"])
        o = parse(plain(failed=["a"]) + plain(passed=["c"]), err)
        self.assertIn("signal: 11", rerun_tests.why_not(o))

    def test_a_failed_test_and_a_doctest_compile_error_get_no_rerun(self):
        out = plain(failed=["a"]) + plain(failed=["src/lib.rs - f (line 3)"])
        err = cargo(1, ["a"], ["   Doc-tests app",
                               "error: doctest failed, to rerun pass `-p app --doc`"])
        o = parse(out, err)
        self.assertEqual(rerun_tests.why_not(o), "a doctest failed")
        # rustdoc that fails to build the doctests at all: a cargo error.
        err = cargo(1, ["a"], ["   Doc-tests app", "error: unresolved import `app::gone`"])
        o = parse(plain(failed=["a"]), err)
        self.assertIn("not a failed test", rerun_tests.why_not(o))

    def test_a_failed_run_without_a_failed_result_gets_no_rerun(self):
        # Two runs failed, but only one printed a failed test result (the
        # other exited 101 after its tests passed).
        o = parse(plain(failed=["a"]) + plain(passed=["b"]), cargo(2, ["a", "b"]))
        self.assertIn("2 test runs failed, but 1", rerun_tests.why_not(o))

    def test_more_than_n_failures_get_no_rerun(self):
        many = parse(plain(failed=[f"t{i}" for i in range(6)]), cargo(1, ["a"]))
        self.assertIn("more than 5", rerun_tests.why_not(many))
        five = parse(plain(failed=[f"t{i}" for i in range(5)]), cargo(1, ["a"]))
        self.assertIsNone(rerun_tests.why_not(five))

    def test_a_libtest_run_inside_a_json_suite_is_output_not_a_binary(self):
        # 0.70.0 cut: "63 runs, 64 started, 64 with a result".
        out = json_suite(passed=["git_identity::tests::instance_identity_from_env"],
                         inside=NESTED) + json_suite(failed=["x::flaky"])
        o = parse(out, cargo(2, ["it"]))
        self.assertEqual((o.runs, o.started, o.finished, o.failed_runs, o.failed_suites),
                         (2, 2, 2, 1, 1))
        self.assertEqual(list(o.failed), ["x::flaky"])
        self.assertIsNone(rerun_tests.why_not(o))

    def test_a_libtest_run_inside_a_plain_suite_is_output_not_a_binary(self):
        # 0.66.0 and 0.67.0, the uid 1001 pass: "1 runs, 2 started, 2 with a result".
        o = parse(with_nested(ONE_FAIL))
        o.ended(101)
        self.assertEqual((o.runs, o.started, o.finished, o.failed_runs, o.failed_suites),
                         (1, 1, 1, 1, 1))
        self.assertEqual(list(o.failed), ["a::flaky"])
        self.assertIn("timed out", o.failed["a::flaky"])
        self.assertIsNone(rerun_tests.why_not(o))

    def test_parent_failures_before_inside_and_after_a_nested_run_are_all_kept(self):
        # Review rev-1222 finding 1: libtest runs tests in parallel, so the
        # parent's own results come between the inner lines.
        inner = INNER_START + failed_events("b") + INNER_END
        out = json_suite(passed=["p"], failed=["a", "c"], inside=inner, at=2, count=3)
        o = parse(out, cargo(1, ["lib"]))
        self.assertEqual(sorted(o.failed), ["a", "b", "c"])
        self.assertEqual((o.started, o.finished, o.miscounted), (1, 1, []))
        self.assertIsNone(rerun_tests.why_not(o))
        # Plain output, the uid 1001 pass.
        out = ("running 4 tests\ntest a ... FAILED\n" + INNER_START + "test b ... FAILED\n"
               + INNER_END + "test c ... FAILED\ntest p ... ok\n\n"
               "test result: FAILED. 1 passed; 3 failed; 0 ignored\n")
        o = parse(out)
        o.ended(101)
        self.assertEqual(sorted(o.failed), ["a", "b", "c"])
        self.assertIsNone(rerun_tests.why_not(o))

    def test_six_failures_with_one_inside_a_nested_run_get_no_rerun(self):
        inner = INNER_START + failed_events("t5") + INNER_END
        out = json_suite(failed=[f"t{i}" for i in range(5)], inside=inner, at=2, count=6)
        o = parse(out, cargo(1, ["lib"]))
        self.assertEqual(len(o.failed), 6)
        self.assertIn("6 tests failed, more than 5", rerun_tests.why_not(o))

    def test_failures_after_a_crashed_nested_run_are_kept(self):
        # The inner run printed `running` and no result: JSON closes the parent anyway.
        out = json_suite(passed=["p"], failed=["a", "b"], inside=INNER_START, at=1)
        o = parse(out, cargo(1, ["lib"]))
        self.assertEqual((o.started, o.finished, sorted(o.failed)), (1, 1, ["a", "b"]))
        self.assertIsNone(rerun_tests.why_not(o))
        # Plain: the parent's result closes only the inner run, so no rerun.
        o = parse("running 2 tests\n" + INNER_START + "test a ... FAILED\n"
                  "test result: FAILED. 1 passed; 1 failed; 0 ignored\n")
        o.ended(101)
        self.assertIn("ended without a result", rerun_tests.why_not(o))

    def test_a_result_that_names_fewer_or_more_failures_gets_no_rerun(self):
        # A failed test that the parser did not see: the count says 2, one is named.
        o = parse(json_suite(failed=["a"], count=2), cargo(1, ["lib"]))
        self.assertIn("2 failed, but 1 failed tests named", rerun_tests.why_not(o))
        # An inner run that fails another test than its parent's.
        inner = INNER_START.replace("... ok", "... FAILED") + INNER_END
        o = parse(json_suite(failed=["a"], inside=inner), cargo(1, ["lib"]))
        self.assertIn("1 failed, but 2 failed tests named", rerun_tests.why_not(o))
        # A plain result without its count.
        o = parse("running 1 test\ntest a ... FAILED\n\ntest result: FAILED. odd\n")
        o.ended(101)
        self.assertIn("no count", rerun_tests.why_not(o))

    def test_refusals_stay_with_a_nested_run(self):
        nested = json_suite(passed=["p"], inside=NESTED)
        # A crash before running: cargo started 3 runs, 2 have a result.
        o = parse(nested + json_suite(failed=["x"]), cargo(3, ["it", "b"]))
        self.assertIn("ended without a result", rerun_tests.why_not(o))
        # A signal.
        err = cargo(2, ["it", "b"], ["  process didn't exit successfully: `b-1`"
                                     " (signal: 11, SIGSEGV: invalid memory reference)"])
        o = parse(nested + json_suite(failed=["x"]), err)
        self.assertIn("signal: 11", rerun_tests.why_not(o))
        # A panic before the tests (exit 101 without a result).
        o = parse(nested + json_suite(failed=["x"]), cargo(3, ["it", "b"]))
        self.assertIn("ended without a result", rerun_tests.why_not(o))
        # More than 5 failed tests.
        o = parse(nested + json_suite(failed=[f"t{i}" for i in range(6)]), cargo(2, ["it"]))
        self.assertIn("more than 5", rerun_tests.why_not(o))
        # A binary that crashed in its tests: a suite without its result.
        cut = json_suite(passed=["p"], inside=NESTED).rsplit("{", 1)[0]
        o = parse(cut + json_suite(failed=["x"]), cargo(2, ["it", "b"]))
        self.assertIn("ended without a result", rerun_tests.why_not(o))

    def test_the_context(self):
        for env, want in ((RELEASE, "release"), (MERGE, "merge"), ({}, None),
                          ({"TERRARIUM_VALIDATE_CONTEXT": "card"}, None)):
            with mock.patch.dict(os.environ, env, clear=True):
                self.assertEqual(rerun_tests.context(), want)


class CargoStep(unittest.TestCase):
    """`ci::test` in a release validation: `rust_tests.run` with a fake cargo."""

    def run_with(self, runs, env=RELEASE, args=("--workspace", "--no-fail-fast")):
        """`runs`: (stdout, stderr, exit code) of the first run and the rerun."""
        calls = []

        def reported(cmd, results, env_, filename="doctests.json", outcomes=None):
            calls.append((filename, cmd))
            out, err, code = runs[len(calls) - 1]
            if outcomes is not None:
                for line in out.splitlines(keepends=True):
                    outcomes.feed(line)
                for line in err.splitlines(keepends=True):
                    outcomes.cargo(line)
            return code

        printed = io.StringIO()
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.dict(os.environ, dict(env, RESULTS_DIR=d), clear=True), \
                mock.patch.object(rust_tests, "reported", side_effect=reported), \
                contextlib.redirect_stdout(printed):
            code = rust_tests.run(list(args))
            path = Path(d, rerun_tests.REPORT)
            report = json.loads(path.read_text()) if path.exists() else None
        return code, calls, printed.getvalue(), report

    def test_a_pass_on_rerun_is_green_and_recorded(self):
        code, calls, out, report = self.run_with([
            (PLAIN_FAIL, cargo(2, ["a"]), 101),
            (plain(passed=["a::flaky"]), cargo(2), 0)])
        self.assertEqual(code, 0)
        self.assertEqual(calls[1], ("rust-rerun.json", [
            "cargo", "test", "--workspace", "--no-fail-fast", "--tests", "--",
            *rust_tests.JSON, "a::flaky", "--exact"]))
        self.assertEqual(report["context"], "release")
        [step] = report["steps"]
        self.assertEqual((step["step"], step["outcome"]), ("test", "green"))
        [test] = step["tests"]
        self.assertEqual((test["name"], test["first"], test["rerun"]), ("a::flaky", "failed", "ok"))
        self.assertIn("timed out", test["tail"])
        self.assertIn("passed on the rerun", out)

    def test_a_test_name_filter_keeps_one_libtest_separator(self):
        # TER-1261: `-- webhooks::` plus a second `-- <JSON options>` made
        # `json` a name filter, and tests outside `webhooks::` ran.
        code, calls, _, _ = self.run_with([
            (PLAIN_FAIL, cargo(2, ["a"]), 101),
            (plain(passed=["a::flaky"]), cargo(2), 0)],
            args=("-p", "terrariumd", "--test", "it", "--no-fail-fast", "--", "webhooks::"))
        self.assertEqual(code, 0)
        self.assertEqual(calls[0], ("rust.json", [
            "cargo", "test", "-p", "terrariumd", "--test", "it", "--no-fail-fast", "--",
            *rust_tests.JSON, "webhooks::"]))
        self.assertEqual(calls[1], ("rust-rerun.json", [
            "cargo", "test", "-p", "terrariumd", "--test", "it", "--no-fail-fast", "--",
            *rust_tests.JSON, "a::flaky", "--exact", "webhooks::"]))

    def test_a_forged_rerun_line_changes_nothing(self):
        # The test output prints the old marker line and a fake JSON result;
        # the record has only the test that this script saw fail and reran.
        out = PLAIN_FAIL.replace("timed out", f"timed out\n{FORGED}")
        _, _, _, report = self.run_with([
            (out, cargo(2, ["a"]), 101),
            (plain(passed=["a::flaky"]) + FORGED + "\n", cargo(2), 0)])
        [step] = report["steps"]
        self.assertEqual([t["name"] for t in step["tests"]], ["a::flaky"])
        self.assertEqual((step["outcome"], step["tests"][0]["rerun"]), ("green", "ok"))
        # A green run writes no record at all, whatever its output says.
        code, calls, _, report = self.run_with([(plain(passed=["a"]) + FORGED + "\n", cargo(1), 0)])
        self.assertEqual((code, len(calls), report), (0, 1, None))

    def test_one_failed_assertion_beside_a_nested_run_is_rerun_once(self):
        # TER-1222: the release suite with one flaky assertion reruns it by itself.
        first = json_suite(passed=["git_identity::tests::instance_identity_from_env"],
                           inside=NESTED) + json_suite(failed=["x::flaky"])
        code, calls, out, report = self.run_with([
            (first, cargo(2, ["it"]), 101),
            (json_suite(passed=["x::flaky"]), cargo(2), 0)])
        self.assertEqual((code, len(calls)), (0, 2))
        self.assertEqual(calls[1][1][-2:], ["x::flaky", "--exact"])
        [step] = report["steps"]
        self.assertEqual((step["outcome"], [(t["name"], t["rerun"]) for t in step["tests"]]),
                         ("green", [("x::flaky", "ok")]))
        self.assertNotIn("no rerun", out)

    def test_a_second_failure_stays_red(self):
        code, calls, out, report = self.run_with([
            (PLAIN_FAIL, cargo(2, ["a"]), 101),
            (plain(failed=["a::flaky"]), cargo(1, ["a"]), 101)])
        self.assertEqual((code, len(calls)), (101, 2))
        self.assertEqual(report["steps"][0]["outcome"], "red")
        self.assertEqual(report["steps"][0]["tests"][0]["rerun"], "failed")
        self.assertIn("failed again: a::flaky (failed)", out)

    def test_a_test_missing_from_the_rerun_stays_red(self):
        code, _, _, report = self.run_with([
            (PLAIN_FAIL, cargo(2, ["a"]), 101), (plain(passed=["other"]), cargo(1), 0)])
        self.assertEqual(code, 101)
        self.assertEqual(report["steps"][0]["tests"][0]["rerun"], "missing")

    def test_a_crash_in_the_rerun_stays_red(self):
        err = cargo(1, ["a"], ["  process didn't exit successfully: `x` (signal: 6, SIGABRT)"])
        code, _, _, report = self.run_with([
            (PLAIN_FAIL, cargo(2, ["a"]), 101), (plain(passed=["a::flaky"]), err, 101)])
        self.assertEqual((code, report["steps"][0]["outcome"]), (101, "red"))

    def test_no_rerun_is_recorded_with_its_reason(self):
        code, calls, out, report = self.run_with([
            (plain(failed=["a"]), cargo(2, ["a", "b"]), 101)])
        self.assertEqual((code, len(calls)), (101, 1))
        self.assertIn("no rerun: a test run ended without a result", out)
        [step] = report["steps"]
        self.assertEqual(step["outcome"], "no_rerun")
        self.assertIn("without a result", step["reason"])
        self.assertEqual([(t["name"], t["rerun"]) for t in step["tests"]], [("a", None)])

    def test_cargo_killed_or_odd_status_stays_red(self):
        # Valid failed-test output and cargo stderr, but cargo itself ends
        # with a signal (-9) or a status other than 101: no rerun, red.
        for status, want in ((-9, "signal 9"), (1, "exit code 1"), (102, "exit code 102")):
            code, calls, out, report = self.run_with([(PLAIN_FAIL, cargo(2, ["a"]), status)])
            self.assertEqual((code, len(calls)), (status, 1), status)
            self.assertIn(f"no rerun: cargo itself ended with {want}, not 101", out)
            [step] = report["steps"]
            self.assertEqual(step["outcome"], "no_rerun")
            self.assertEqual([t["rerun"] for t in step["tests"]], [None])

    def test_more_than_n_failures_get_no_rerun(self):
        code, calls, out, _ = self.run_with([
            (plain(failed=[f"t{i}" for i in range(6)]), cargo(1, ["a"]), 101)])
        self.assertEqual((code, len(calls)), (101, 1))
        self.assertIn("no rerun: 6 tests failed, more than 5", out)

    def test_a_merge_job_reruns_too(self):
        code, _, _, report = self.run_with([
            (PLAIN_FAIL, cargo(2, ["a"]), 101), (plain(passed=["a::flaky"]), cargo(2), 0)],
            env=MERGE)
        self.assertEqual((code, report["context"]), (0, "merge"))

    def test_a_selected_target_reruns_without_adding_tests(self):
        _, calls, _, _ = self.run_with([
            (PLAIN_FAIL, cargo(2, ["a"]), 101), (plain(passed=["a::flaky"]), cargo(1), 0)],
            args=("-p", "x", "--test", "it"))
        self.assertEqual(calls[1][1][:7], ["cargo", "test", "-p", "x", "--test", "it", "--"])

    def test_a_card_check_does_not_rerun(self):
        code, calls, _, report = self.run_with([(PLAIN_FAIL, cargo(2, ["a"]), 101)], env={})
        self.assertEqual((code, len(calls), report), (101, 1, None))

    def test_stderr_of_cargo_is_tracked_and_still_printed(self):
        script = ("import sys; print('running 1 test'); print('test a ... FAILED');"
                  " print('test result: FAILED. 0 passed; 1 failed');"
                  " print('     Running unittests src/lib.rs (x)', file=sys.stderr);"
                  " print('error: test failed, to rerun pass `-p app --lib`', file=sys.stderr);"
                  " sys.exit(101)")
        o = rerun_tests.Outcomes()
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            code = rust_tests.teed([sys.executable, "-c", script], None, None, o)
        self.assertEqual(code, 101)
        self.assertEqual((o.runs, o.failed_runs, o.tracked), (1, 1, True))
        self.assertIn("error: test failed", err.getvalue())
        self.assertIsNone(rerun_tests.why_not(o.done()))


class UnprivilegedStep(unittest.TestCase):
    """The uid 1001 pass: each failed binary runs its failed tests again."""

    def first(self, out, code=101):
        o = parse(out)
        o.ended(code)
        return o

    def rerun(self, failed, outputs, codes):
        calls = []

        def run(cmd, **kw):
            calls.append((cmd, kw["cwd"], kw["env"]["HOME"]))
            n = len(calls) - 1
            return mock.Mock(stdout=outputs[n], returncode=codes[n])

        printed = io.StringIO()
        with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as results, \
                mock.patch.dict(os.environ, dict(RELEASE, RESULTS_DIR=results), clear=True), \
                mock.patch.object(unprivileged_tests.subprocess, "run", side_effect=run), \
                mock.patch.object(unprivileged_tests.os, "chown"), \
                contextlib.redirect_stdout(printed):
            ok = unprivileged_tests.rerun(failed, root, 4, [], results)
            report = json.loads(Path(results, rerun_tests.REPORT).read_text())
        return ok, calls, printed.getvalue(), report

    def test_a_pass_on_rerun_as_uid_1001_is_green(self):
        failed = [("/t/it-1", "/w/crates/d", self.first(ONE_FAIL))]
        ok, calls, _, report = self.rerun(failed, [plain(passed=["a::flaky"])], [0])
        self.assertTrue(ok)
        cmd, cwd, home = calls[0]
        self.assertEqual(cmd, unprivileged_tests.setpriv(1001) + ["/t/it-1", "a::flaky", "--exact"])
        self.assertEqual(cwd, "/w/crates/d")
        self.assertTrue(home.endswith("/4h"))
        [step] = report["steps"]
        self.assertEqual((step["step"], step["outcome"], step["tests"][0]["rerun"]),
                         ("test-unprivileged", "green", "ok"))

    def test_one_failed_assertion_beside_a_nested_run_as_uid_1001_is_rerun_once(self):
        failed = [("/t/lib-1", "/w", self.first(with_nested(ONE_FAIL)))]
        ok, calls, out, report = self.rerun(failed, [plain(passed=["a::flaky"])], [0])
        self.assertTrue(ok)
        self.assertEqual(len(calls), 1)
        self.assertNotIn("no rerun", out)
        self.assertEqual(report["steps"][0]["outcome"], "green")

    def test_a_second_failure_as_uid_1001_stays_red(self):
        failed = [("/t/it-1", "/w", self.first(ONE_FAIL)),
                  ("/t/lib-2", "/w", self.first(plain(failed=["c"])))]
        ok, calls, _, report = self.rerun(
            failed, [plain(passed=["a::flaky"]), plain(failed=["c"])], [0, 101])
        self.assertFalse(ok)
        self.assertEqual(len(calls), 2)
        self.assertEqual({t["name"]: t["rerun"] for t in report["steps"][0]["tests"]},
                         {"a::flaky": "ok", "c": "failed"})

    def test_more_than_n_failures_as_uid_1001_get_no_rerun(self):
        failed = [("/t/a", "/w", self.first(plain(failed=["a1", "a2", "a3"]))),
                  ("/t/b", "/w", self.first(plain(failed=["b1", "b2", "b3"])))]
        ok, calls, out, report = self.rerun(failed, [], [])
        self.assertFalse(ok)
        self.assertEqual(calls, [])
        self.assertIn("no rerun: 6 tests failed", out)
        self.assertEqual(report["steps"][0]["outcome"], "no_rerun")

    def test_a_crash_before_running_as_uid_1001_gets_no_rerun(self):
        # A binary that aborts before its tests: exit 101 and no results.
        failed = [("/t/a", "/w", self.first(ONE_FAIL)), ("/t/b", "/w", self.first(""))]
        ok, calls, _, _ = self.rerun(failed, [], [])
        self.assertFalse(ok)
        self.assertEqual(calls, [])

    def test_a_signal_or_odd_exit_as_uid_1001_gets_no_rerun(self):
        for code, want in ((-11, "signal 11"), (2, "exit code 2")):
            failed = [("/t/a", "/w", self.first(ONE_FAIL, code))]
            ok, calls, out, _ = self.rerun(failed, [], [])
            self.assertFalse(ok)
            self.assertEqual(calls, [])
            self.assertIn(want, out)


class FlakyCards(unittest.TestCase):
    def test_only_tests_that_passed_on_the_rerun_get_a_card_command(self):
        data = {"version": 1, "context": "release", "steps": [
            {"step": "test", "outcome": "green", "tests": [
                {"name": "a::flaky", "first": "failed", "rerun": "ok", "tail": "timed out\n'quoted'"}]},
            {"step": "test-unprivileged", "outcome": "red", "tests": [
                {"name": "b::broken", "first": "failed", "rerun": "failed", "tail": ""}]}]}
        with tempfile.TemporaryDirectory() as d:
            path = Path(d, "reruns.json")
            path.write_text(json.dumps(data))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(flaky_cards.main([str(path), "--source", "the 0.43.0 cut"]), 0)
        text = out.getvalue()
        tag = flaky_cards.label("a::flaky")
        self.assertIn(f"terrarium card ls --label {tag} --json", text)
        self.assertIn("terrarium card new 'Flaky test: a::flaky' --label flaky", text)
        self.assertIn("--label version:vnext --label type:small", text)
        self.assertIn("the 0.43.0 cut", text)
        self.assertNotIn("b::broken", text)
        # The output is valid shell (quotes in the tail do not break it).
        subprocess.run(["sh", "-n"], input=text, text=True, check=True)
        self.assertNotEqual(flaky_cards.label("a::flaky"), flaky_cards.label("a::flakz"))

    def test_no_flaky_test_prints_no_command(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d, "reruns.json")
            path.write_text(json.dumps({"steps": []}))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(flaky_cards.main([str(path)]), 0)
        self.assertNotIn("terrarium card", out.getvalue())


if __name__ == "__main__":
    unittest.main()
