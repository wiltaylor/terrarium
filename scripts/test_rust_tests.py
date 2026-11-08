"""Regression checks for the shared Rust CI runner."""

import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import rust_tests


class RustTests(unittest.TestCase):
    def test_whole_workspace_keeps_doctests_after_failure(self):
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(
            rust_tests.subprocess, "call", side_effect=[0, 1, 0]
        ) as call:
            self.assertEqual(rust_tests.run(["--workspace", "--no-fail-fast"]), 1)
        self.assertEqual(call.call_args_list[1].args[0], [
            "cargo", "nextest", "run", "--profile", "ci", "--workspace", "--no-fail-fast"])
        self.assertEqual(call.call_args_list[2].args[0], [
            "cargo", "test", "--doc", "--workspace", "--no-fail-fast"])

    def test_selected_target_does_not_add_doctests(self):
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(
            rust_tests.subprocess, "call", return_value=0
        ) as call:
            self.assertEqual(rust_tests.run(["-p", "terrariumd", "--test", "jobs"]), 0)
        self.assertEqual(call.call_count, 2)

    def test_results_keep_nextest_junit_and_doctest_json_separate(self):
        calls = []

        def reported(cmd, results, env):
            calls.append((cmd, env))
            with Path(results, "doctests.json").open("ab") as out:
                out.write(b'{"type":"suite"}\n')
            return 0

        with tempfile.TemporaryDirectory() as d, mock.patch.dict(
            os.environ, {"RESULTS_DIR": d}, clear=True
        ), mock.patch.object(rust_tests.subprocess, "call", return_value=0), \
                mock.patch.object(rust_tests, "reported", side_effect=reported), \
                mock.patch.object(rust_tests, "junit_source", return_value=Path(d, "source.xml")), \
                mock.patch.object(rust_tests, "copy_junit") as junit:
            rust_tests.run(["--workspace"])
            self.assertEqual(Path(d, "doctests.json").read_bytes().count(b"\n"), 1)
            junit.assert_called_once_with(d, Path(d, "source.xml"))
        self.assertEqual(calls[0][1]["RUSTC_BOOTSTRAP"], "1")

    def test_failed_result_output_is_also_in_the_job_log(self):
        failure = '{"type":"test","event":"failed","name":"early_failure","stdout":"panicked: expected value"}\n'
        child = mock.MagicMock()
        child.__enter__.return_value = child
        child.stdout = io.StringIO(failure)
        child.wait.return_value = 101
        with tempfile.TemporaryDirectory() as d, mock.patch.dict(
            os.environ, {"RESULTS_DIR": d}, clear=True
        ), mock.patch.object(rust_tests.subprocess, "call", return_value=0), \
                mock.patch.object(rust_tests.subprocess, "Popen", return_value=child), \
                mock.patch.object(rust_tests.sys, "stdout", new_callable=io.StringIO) as console:
            Path(d, "doctests.json").write_text("old result\n")
            self.assertEqual(rust_tests.reported(["cargo", "test", "--doc"], d, {}), 101)
            self.assertIn(failure, console.getvalue())
            self.assertNotIn("old result", console.getvalue())
            self.assertEqual(Path(d, "doctests.json").read_text(), "old result\n" + failure)

    def test_install_failure_stops_run(self):
        with mock.patch.object(rust_tests.subprocess, "call", return_value=7) as call:
            self.assertEqual(rust_tests.run(["--workspace"]), 7)
        self.assertEqual(call.call_count, 1)

    def test_junit_store_is_independent_of_cargo_target_directory(self):
        with mock.patch.object(rust_tests.subprocess, "check_output", return_value=
                               '{"workspace_root":"/repo","target_directory":"/build"}'):
            self.assertEqual(rust_tests.junit_source(), Path("/repo/target/nextest/ci/junit.xml"))

    def test_affected_invocations_preserve_each_junit_report(self):
        with tempfile.TemporaryDirectory() as d:
            source = Path(d, "source.xml")
            source.write_text("first")
            rust_tests.copy_junit(d, source)
            source.write_text("second")
            rust_tests.copy_junit(d, source)
            self.assertEqual(Path(d, "rust.xml").read_text(), "first")
            self.assertEqual(Path(d, "rust-2.xml").read_text(), "second")
