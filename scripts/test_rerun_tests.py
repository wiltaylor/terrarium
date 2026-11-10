"""The rerun of the failed tests of a merge job (TER-683)."""

import contextlib
import io
import json
import os
import tempfile
import unittest
from unittest import mock

import rerun_tests
import rust_tests
import unprivileged_tests

MERGE = {"TERRARIUM_JOB_KIND": "merge"}

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


def plain(passed=(), failed=()):
    lines = [f"running {len(passed) + len(failed)} tests"]
    lines += [f"test {n} ... ok" for n in passed]
    lines += [f"test {n} ... FAILED" for n in failed]
    state = "FAILED" if failed else "ok"
    lines.append(f"test result: {state}. {len(passed)} passed; {len(failed)} failed; 0 ignored")
    return "".join(line + "\n" for line in lines)


def parse(text):
    o = rerun_tests.Outcomes()
    for line in text.splitlines(keepends=True):
        o.feed(line)
    return o.done()


def markers(out):
    return [json.loads(line[len(rerun_tests.MARKER):]) for line in out.splitlines()
            if line.startswith(rerun_tests.MARKER)]


class Parse(unittest.TestCase):
    def test_plain_output_gives_the_failed_tests_and_their_output(self):
        o = parse(PLAIN_FAIL)
        self.assertEqual((o.started, o.finished), (2, 2))
        self.assertEqual(list(o.failed), ["a::flaky"])
        self.assertIn("timed out", o.failed["a::flaky"])
        self.assertIn("b::fine", o.passed)
        self.assertIsNone(rerun_tests.why_not(o))

    def test_libtest_json_gives_the_failed_tests(self):
        events = [
            {"type": "suite", "event": "started", "test_count": 2},
            {"type": "test", "event": "started", "name": "x"},
            {"type": "test", "event": "failed", "name": "x", "stdout": "boom"},
            {"type": "test", "event": "ok", "name": "y"},
            {"type": "suite", "event": "failed", "passed": 1, "failed": 1},
        ]
        o = parse("".join(json.dumps(e) + "\n" for e in events))
        self.assertEqual(o.failed, {"x": "boom"})
        self.assertEqual(o.passed, {"y"})
        self.assertIsNone(rerun_tests.why_not(o))

    def test_no_rerun_without_an_explained_failure(self):
        build = parse("error[E0425]: cannot find value `x` in this scope\nerror: could not compile\n")
        self.assertIn("no test failed", rerun_tests.why_not(build))
        crash = parse(plain(failed=["a"]) + "running 4 tests\ntest c ... ok\n")
        self.assertIn("without a result", rerun_tests.why_not(crash))
        doc = parse(plain(failed=["src/lib.rs - f (line 3)"]))
        self.assertEqual(rerun_tests.why_not(doc), "a doctest failed")
        many = parse(plain(failed=[f"t{i}" for i in range(rerun_tests.MAX_RERUN + 1)]))
        self.assertIn("more than 5", rerun_tests.why_not(many))
        five = parse(plain(failed=[f"t{i}" for i in range(rerun_tests.MAX_RERUN)]))
        self.assertIsNone(rerun_tests.why_not(five))


class CargoStep(unittest.TestCase):
    """`ci::test` in a merge job: `rust_tests.run` with a fake cargo."""

    def run_with(self, outputs, codes, env=MERGE):
        calls = []

        def teed(cmd, env_, out=None, outcomes=None):
            calls.append(cmd)
            text = outputs[len(calls) - 1]
            for line in text.splitlines(keepends=True):
                if outcomes is not None:
                    outcomes.feed(line)
            return codes[len(calls) - 1]

        printed = io.StringIO()
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch.object(rust_tests, "teed", side_effect=teed), \
                mock.patch.object(rust_tests.subprocess, "call", return_value=codes[0]) as call, \
                contextlib.redirect_stdout(printed):
            code = rust_tests.run(["--workspace", "--no-fail-fast"])
        return code, calls, printed.getvalue(), call

    def test_a_pass_on_rerun_is_green_and_names_the_flaky_test(self):
        code, calls, out, _ = self.run_with([PLAIN_FAIL, plain(passed=["a::flaky"])], [101, 0])
        self.assertEqual(code, 0)
        self.assertEqual(calls[1], ["cargo", "test", "--workspace", "--no-fail-fast", "--tests",
                                    "--", "a::flaky", "--exact"])
        [m] = markers(out)
        self.assertEqual((m["step"], m["test"], m["first"], m["rerun"]),
                         ("test", "a::flaky", "failed", "ok"))
        self.assertIn("timed out", m["tail"])
        self.assertIn("passed on the rerun", out)

    def test_a_second_failure_stays_red(self):
        code, calls, out, _ = self.run_with([PLAIN_FAIL, plain(failed=["a::flaky"])], [101, 101])
        self.assertEqual(code, 101)
        self.assertEqual(len(calls), 2)
        [m] = markers(out)
        self.assertEqual(m["rerun"], "failed")
        self.assertIn("failed again: a::flaky (failed)", out)

    def test_a_test_missing_from_the_rerun_stays_red(self):
        code, _, out, _ = self.run_with([PLAIN_FAIL, plain(passed=["other"])], [101, 0])
        self.assertEqual(code, 101)
        self.assertEqual(markers(out)[0]["rerun"], "missing")

    def test_more_than_n_failures_get_no_rerun(self):
        many = plain(failed=[f"t{i}" for i in range(6)])
        code, calls, out, _ = self.run_with([many], [101])
        self.assertEqual((code, len(calls)), (101, 1))
        self.assertIn("no rerun: 6 tests failed, more than 5", out)
        self.assertEqual(markers(out), [])

    def test_a_build_error_gets_no_rerun(self):
        code, calls, out, _ = self.run_with(["error: could not compile `x`\n"], [101])
        self.assertEqual((code, len(calls)), (101, 1))
        self.assertIn("no rerun", out)

    def test_a_selected_target_reruns_without_adding_tests(self):
        calls = []

        def teed(cmd, env_, out=None, outcomes=None):
            calls.append(cmd)
            for line in (PLAIN_FAIL if len(calls) == 1 else plain(passed=["a::flaky"])).splitlines():
                outcomes.feed(line)
            return 101 if len(calls) == 1 else 0

        with mock.patch.dict(os.environ, MERGE, clear=True), \
                mock.patch.object(rust_tests, "teed", side_effect=teed), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(rust_tests.run(["-p", "x", "--test", "it"]), 0)
        self.assertEqual(calls[1], ["cargo", "test", "-p", "x", "--test", "it", "--",
                                    "a::flaky", "--exact"])

    def test_a_card_check_does_not_rerun(self):
        code, calls, _, call = self.run_with([PLAIN_FAIL], [101], env={})
        self.assertEqual((code, calls), (101, []))
        call.assert_called_once_with(["cargo", "test", "--workspace", "--no-fail-fast"])

    def test_results_keep_the_rerun_json_apart(self):
        seen = []

        def reported(cmd, results, env, filename="doctests.json", outcomes=None):
            seen.append((filename, cmd[-2:]))
            text = PLAIN_FAIL if len(seen) == 1 else plain(passed=["a::flaky"])
            for line in text.splitlines():
                outcomes.feed(line)
            return 101 if len(seen) == 1 else 0

        with tempfile.TemporaryDirectory() as d, \
                mock.patch.dict(os.environ, dict(MERGE, RESULTS_DIR=d), clear=True), \
                mock.patch.object(rust_tests, "reported", side_effect=reported), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(rust_tests.run(["--workspace"]), 0)
        self.assertEqual(seen, [("rust.json", ["json", "--report-time"]),
                                ("rust-rerun.json", ["a::flaky", "--exact"])])


class UnprivilegedStep(unittest.TestCase):
    """The uid 1001 step: each failed binary runs its failed tests again."""

    def rerun(self, failed, outputs, codes):
        calls = []

        def run(cmd, **kw):
            calls.append((cmd, kw["cwd"], kw["env"]["HOME"]))
            n = len(calls) - 1
            return mock.Mock(stdout=outputs[n], returncode=codes[n])

        printed = io.StringIO()
        with tempfile.TemporaryDirectory() as root, \
                mock.patch.object(unprivileged_tests.subprocess, "run", side_effect=run), \
                mock.patch.object(unprivileged_tests.os, "chown"), \
                contextlib.redirect_stdout(printed):
            ok = unprivileged_tests.rerun(failed, root, 4, [], None)
        return ok, calls, printed.getvalue()

    def test_a_pass_on_rerun_as_uid_1001_is_green(self):
        failed = [("/t/it-1", "/w/crates/d", parse(PLAIN_FAIL))]
        ok, calls, out = self.rerun(failed, [plain(passed=["a::flaky"])], [0])
        self.assertTrue(ok)
        cmd, cwd, home = calls[0]
        self.assertEqual(cmd, unprivileged_tests.setpriv(1001) + ["/t/it-1", "a::flaky", "--exact"])
        self.assertEqual(cwd, "/w/crates/d")
        self.assertTrue(home.endswith("/4h"))
        [m] = markers(out)
        self.assertEqual((m["step"], m["rerun"]), ("test-unprivileged", "ok"))

    def test_a_second_failure_as_uid_1001_stays_red(self):
        failed = [("/t/it-1", "/w", parse(PLAIN_FAIL)),
                  ("/t/lib-2", "/w", parse(plain(failed=["c"])))]
        ok, calls, out = self.rerun(failed, [plain(passed=["a::flaky"]), plain(failed=["c"])],
                                    [0, 101])
        self.assertFalse(ok)
        self.assertEqual(len(calls), 2)
        self.assertEqual({m["test"]: m["rerun"] for m in markers(out)},
                         {"a::flaky": "ok", "c": "failed"})

    def test_more_than_n_failures_as_uid_1001_get_no_rerun(self):
        failed = [("/t/a", "/w", parse(plain(failed=["a1", "a2", "a3"]))),
                  ("/t/b", "/w", parse(plain(failed=["b1", "b2", "b3"])))]
        ok, calls, out = self.rerun(failed, [], [])
        self.assertFalse(ok)
        self.assertEqual(calls, [])
        self.assertIn("no rerun: 6 tests failed", out)

    def test_a_crashed_binary_as_uid_1001_gets_no_rerun(self):
        failed = [("/t/a", "/w", parse(PLAIN_FAIL)), ("/t/b", "/w", parse("running 2 tests\n"))]
        ok, calls, _ = self.rerun(failed, [], [])
        self.assertFalse(ok)
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
