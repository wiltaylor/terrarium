"""Tests for scripts/annotate_test_failures.py and its CI step.

Run: uv run python -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import annotate_test_failures as atf  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "scripts/annotate_test_failures.py"
WORKFLOW = (REPO / ".github/workflows/ci.yml").read_text()

# The shape of `cargo test --workspace --no-fail-fast 2>&1` with two failing
# binaries, colour codes (CARGO_TERM_COLOR=always), and a passing doc-test.
LOG = """\
   Compiling terrariumd v0.1.0 (/home/runner/work/terrarium/terrarium/crates/terrariumd)
    Finished `test` profile [unoptimized] target(s) in 4m 03s
     Running unittests src/lib.rs (target/debug/deps/terrariumd-0123456789abcdef)

running 3 tests
test api::tests::ok_one ... ok
test repo_browse::tests::a_race_in_the_git_dir_never_serves_a_foreign_repo ... \x1b[31mFAILED\x1b[0m
test api::tests::ok_two ... ok

failures:

---- repo_browse::tests::a_race_in_the_git_dir_never_serves_a_foreign_repo stdout ----

thread 'repo_browse::tests::a_race_in_the_git_dir_never_serves_a_foreign_repo' (4242) panicked at crates/terrariumd/src/repo_browse.rs:812:9:
only 29 good reads
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    repo_browse::tests::a_race_in_the_git_dir_never_serves_a_foreign_repo

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.12s

     Running tests/lifecycle.rs (target/debug/deps/lifecycle-fedcba9876543210)

running 2 tests
test max_age_compacts_at_the_next_rest ... FAILED
test health_names_the_idle_stop ... ok

failures:

---- max_age_compacts_at_the_next_rest stdout ----
some output of the test

thread 'max_age_compacts_at_the_next_rest' (99) panicked at crates/terrariumd/tests/lifecycle.rs:640:5:
assertion `left == right` failed
  left: String("working")
 right: "idle"

failures:
    max_age_compacts_at_the_next_rest

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.01s

   Doc-tests terrarium_core

running 1 test
test crates/terrarium-core/src/lib.rs - doc (line 3) ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s

error: test failed, to rerun pass `-p terrariumd --lib`
error: test failed, to rerun pass `-p terrariumd --test lifecycle`
error: 2 targets failed:
"""


class Parse(unittest.TestCase):
    def test_each_failed_test_with_its_binary_and_panic(self):
        got = atf.parse(LOG)
        self.assertEqual(
            [(f.binary, f.name, f.file, f.line, f.message) for f in got],
            [
                (
                    "terrariumd (src/lib.rs)",
                    "repo_browse::tests::a_race_in_the_git_dir_never_serves_a_foreign_repo",
                    "crates/terrariumd/src/repo_browse.rs",
                    812,
                    "only 29 good reads",
                ),
                (
                    "lifecycle (tests/lifecycle.rs)",
                    "max_age_compacts_at_the_next_rest",
                    "crates/terrariumd/tests/lifecycle.rs",
                    640,
                    'assertion `left == right` failed\nleft: String("working")\nright: "idle"',
                ),
            ],
        )

    def test_a_green_log_has_no_failure(self):
        green = "\n".join(l for l in LOG.splitlines() if "FAILED" not in l and "----" not in l)
        self.assertEqual(atf.parse(green), [])

    def test_a_failed_doc_test(self):
        log = (
            "   Doc-tests terrarium_core\n\nrunning 1 test\n"
            "test crates/terrarium-core/src/lib.rs - doc (line 3) ... FAILED\n"
        )
        [f] = atf.parse(log)
        self.assertEqual((f.binary, f.name), ("terrarium_core (doc-tests)", "crates/terrarium-core/src/lib.rs - doc (line 3)"))


class PanicMessage(unittest.TestCase):
    def test_at_most_three_lines_each_cut(self):
        after = ["", "x" * 300, "  left: 200", " right: 503", "more", "", "note: run with"]
        msg = atf.panic_message(after)
        self.assertEqual(msg.split("\n"), ["x" * 197 + "...", "left: 200", "right: 503"])

    def test_it_stops_at_the_note(self):
        self.assertEqual(atf.panic_message(["boom", "note: run with `RUST_BACKTRACE=1`"]), "boom")


class Annotations(unittest.TestCase):
    def test_one_error_per_failure_with_file_line_and_title(self):
        lines = atf.annotations(atf.parse(LOG))
        self.assertEqual(
            lines,
            [
                "::error file=crates/terrariumd/src/repo_browse.rs,line=812,title=terrariumd (src/lib.rs)"
                "::repo_browse::tests::a_race_in_the_git_dir_never_serves_a_foreign_repo FAILED%0Aonly 29 good reads",
                "::error file=crates/terrariumd/tests/lifecycle.rs,line=640,title=lifecycle (tests/lifecycle.rs)"
                "::max_age_compacts_at_the_next_rest FAILED%0Aassertion `left == right` failed"
                '%0Aleft: String("working")%0Aright: "idle"',
            ],
        )

    def test_the_title_and_message_are_escaped(self):
        line = atf.annotation("a, b: c", "50% done\nnext", "x.rs", 3)
        self.assertEqual(line, "::error file=x.rs,line=3,title=a%2C b%3A c::50%25 done%0Anext")

    def test_more_than_ten_failures_keep_ten_annotations_and_every_name(self):
        failures = [atf.Failure("bin (tests/bin.rs)", f"t{i}") for i in range(13)]
        lines = atf.annotations(failures)
        self.assertEqual(len(lines), atf.MAX_ANNOTATIONS)
        self.assertTrue(lines[8].endswith("::t8 FAILED"))
        self.assertIn("title=4 more failed tests::", lines[9])
        for i in range(9, 13):
            self.assertIn(f"bin (tests/bin.rs): t{i} FAILED", lines[9].replace("%0A", "\n"))

    def test_the_script_prints_the_annotations_and_exits_zero(self):
        with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as f:
            f.write(LOG)
        try:
            out = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), f.name], capture_output=True, text=True, check=False
            )
        finally:
            Path(f.name).unlink()
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout.count("::error "), 2)
        self.assertIn("2 failed tests", out.stderr)


class Workflow(unittest.TestCase):
    def test_the_test_step_annotates_failures_and_keeps_the_exit_code(self):
        self.assertIn("just ci::test 2>&1 | tee \"$RUNNER_TEMP/cargo-test.log\" || rc=$?", WORKFLOW)
        self.assertIn("set -o pipefail", WORKFLOW)
        self.assertIn('python3 scripts/annotate_test_failures.py "$RUNNER_TEMP/cargo-test.log" || true', WORKFLOW)
        self.assertIn('exit "$rc"', WORKFLOW)


if __name__ == "__main__":
    unittest.main()
