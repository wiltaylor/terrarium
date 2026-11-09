"""The Docker merge gate must fail closed, without a real VM or key."""

import json
import os
import subprocess
from pathlib import Path
import unittest
from unittest.mock import patch

import docker_ci


class DockerGateTests(unittest.TestCase):
    def test_requires_exactly_one_successful_test(self):
        name = docker_ci.LIVE_TESTS[0]
        def event(name, status):
            return json.dumps({"type": "test", "name": name, "event": status}) + "\n"
        self.assertTrue(docker_ci.passed_exactly(event(name, "started") + event(name, "ok"), name))
        for output in ("", event(name, "ignored"), event(name, "failed"),
                       event("wrong-test", "ok"), event(name, "ok") * 2):
            self.assertFalse(docker_ci.passed_exactly(output, name))

    def test_worker_cannot_enter_gate(self):
        with patch.dict(os.environ, {"TERRARIUM_JOB_KIND": "validate", "TERRARIUM_DOCKER_CI": "1"}):
            with patch.object(docker_ci, "gate") as gate:
                self.assertEqual(docker_ci.main(), 1)
                gate.assert_not_called()

    def test_missing_endpoint_is_a_failure(self):
        with patch.dict(os.environ, {"TERRARIUM_JOB_KIND": "merge", "TERRARIUM_DOCKER_CI": "1"}, clear=True):
            self.assertEqual(docker_ci.main(), 1)

    def test_default_path_needs_no_key_or_docker_and_keeps_full_validation(self):
        for switch in (None, "0"):
            env = {"TERRARIUM_JOB_KIND": "merge"}
            if switch is not None:
                env["TERRARIUM_DOCKER_CI"] = switch
            with patch.dict(os.environ, env, clear=True), \
                 patch.object(subprocess, "call", return_value=17) as validate, \
                 patch.object(subprocess, "Popen") as agent, \
                 patch.object(docker_ci, "gate") as gate:
                self.assertEqual(docker_ci.main(), 17)
                validate.assert_called_once_with(["just", "ci::validate"], env={
                    k: v for k, v in env.items() if True})
                agent.assert_not_called()
                gate.assert_not_called()

    def test_live_tests_fail_when_cargo_selects_nothing_or_ignores_test(self):
        for output in ("", json.dumps({"type": "test", "name": docker_ci.LIVE_TESTS[0], "event": "ignored"})):
            with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0, output)):
                self.assertEqual(docker_ci.live_tests({}), 1)

    def test_live_tests_run_each_named_test_with_ignored_and_exact(self):
        calls = []
        def run(cmd, **kwargs):
            calls.append(cmd)
            name = cmd[cmd.index("--exact") + 1]
            return subprocess.CompletedProcess(cmd, 0, json.dumps({"type": "test", "name": name, "event": "ok"}))
        with patch.object(subprocess, "run", side_effect=run):
            self.assertEqual(docker_ci.live_tests({}), 0)
        self.assertEqual(len(calls), len(docker_ci.LIVE_TESTS))
        self.assertTrue(all("--ignored" in cmd for cmd in calls))

    def test_enabled_gate_gets_only_unix_endpoint_and_isolated_client_config(self):
        endpoint = "unix:///tmp/job/docker.sock"
        env = {"TERRARIUM_JOB_KIND": "merge", "TERRARIUM_DOCKER_CI": "1",
               "DOCKER_HOST": endpoint, "DOCKER_CONTEXT": "wrong-daemon",
               "SSH_AUTH_SOCK": "/not-forwarded"}
        def gate(child):
            self.assertEqual(child["DOCKER_HOST"], endpoint)
            self.assertNotIn("DOCKER_CONTEXT", child)
            self.assertNotIn("SSH_AUTH_SOCK", child)
            self.assertIn("DOCKER_CONFIG", child)
            return 0
        with patch.dict(os.environ, env, clear=True), patch.object(docker_ci, "gate", side_effect=gate):
            self.assertEqual(docker_ci.main(), 0)
        env["DOCKER_HOST"] = "ssh://untrusted"
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(docker_ci.main(), 1)


if __name__ == "__main__":
    unittest.main()
