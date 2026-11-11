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
        with patch.dict(os.environ, {"TERRARIUM_JOB_KIND": "validate", "TERRARIUM_DOCKER_CI": "1", "TERRARIUM_DOCKER_HOST": "unix:///tmp/job/docker.sock"}):
            with patch.object(docker_ci, "gate") as gate:
                self.assertEqual(docker_ci.main(), 1)
                gate.assert_not_called()

    def test_release_e2e_enters_gate_without_credentials(self):
        endpoint = "unix:///tmp/job/docker.sock"
        with patch.dict(os.environ, {"TERRARIUM_JOB_KIND": "e2e", "TERRARIUM_DOCKER_CI": "1",
                                     "TERRARIUM_DOCKER_HOST": endpoint}, clear=True), \
             patch.object(docker_ci, "gate", return_value=0) as gate:
            self.assertEqual(docker_ci.main(), 0)
        child = gate.call_args.args[0]
        self.assertEqual(child["DOCKER_HOST"], endpoint)
        self.assertNotIn("TERRARIUM_DOCKER_HOST", child)
        self.assertNotIn("DOCKER_CI_KEY", child)

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

    def test_gate_off_ignores_docker_endpoints(self):
        # TER-477: with merge secrets set the daemon provides an endpoint even
        # when the gate is off; the suite must not see it.
        for switch in (None, "0"):
            env = {"TERRARIUM_JOB_KIND": "merge", "DOCKER_HOST": "unix:///tmp/x/docker.sock",
                   "TERRARIUM_DOCKER_HOST": "unix:///tmp/job/docker.sock", "KEEP": "1"}
            if switch is not None:
                env["TERRARIUM_DOCKER_CI"] = switch
            with patch.dict(os.environ, env, clear=True), \
                 patch.object(subprocess, "call", return_value=0) as validate, \
                 patch.object(docker_ci, "gate") as gate:
                self.assertEqual(docker_ci.main(), 0)
                child = validate.call_args.kwargs["env"]
                self.assertNotIn("DOCKER_HOST", child)
                self.assertNotIn("TERRARIUM_DOCKER_HOST", child)
                self.assertEqual(child["KEEP"], "1")
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

    def test_gate_keeps_docker_env_out_of_the_suite(self):
        # TER-518: the suite's own test daemons and recipes must not see the
        # outer merge job's gate switch or socket; only `docker info` and the
        # live tests get DOCKER_HOST.
        endpoint = "unix:///tmp/job/docker.sock"
        env = {"TERRARIUM_JOB_KIND": "merge", "TERRARIUM_DOCKER_CI": "1",
               "TERRARIUM_DOCKER_HOST": endpoint, "DOCKER_HOST": endpoint,
               "DOCKER_CONFIG": "/tmp/config", "KEEP": "1"}
        with patch.object(subprocess, "run") as info, \
             patch.object(subprocess, "call", return_value=0) as validate, \
             patch.object(docker_ci, "live_tests", return_value=0) as live:
            self.assertEqual(docker_ci.gate(dict(env)), 0)
        self.assertEqual(info.call_args.kwargs["env"]["DOCKER_HOST"], endpoint)
        validate.assert_called_once()
        self.assertEqual(validate.call_args.args[0], ["just", "ci::validate"])
        suite = validate.call_args.kwargs["env"]
        for name in ("TERRARIUM_DOCKER_CI", "TERRARIUM_DOCKER_HOST", "DOCKER_HOST", "DOCKER_CONFIG"):
            self.assertNotIn(name, suite)
        self.assertEqual(suite["KEEP"], "1")
        self.assertEqual(suite["TERRARIUM_JOB_KIND"], "merge")
        self.assertEqual(live.call_args.args[0]["DOCKER_HOST"], endpoint)

    def test_enabled_gate_gets_only_unix_endpoint_and_isolated_client_config(self):
        endpoint = "unix:///tmp/job/docker.sock"
        env = {"TERRARIUM_JOB_KIND": "merge", "TERRARIUM_DOCKER_CI": "1",
               "TERRARIUM_DOCKER_HOST": endpoint, "DOCKER_HOST": "unix:///inherited.sock",
               "DOCKER_CONTEXT": "wrong-daemon", "SSH_AUTH_SOCK": "/not-forwarded"}
        def gate(child):
            self.assertEqual(child["DOCKER_HOST"], endpoint)
            self.assertNotIn("TERRARIUM_DOCKER_HOST", child)
            self.assertNotIn("DOCKER_CONTEXT", child)
            self.assertNotIn("SSH_AUTH_SOCK", child)
            self.assertIn("DOCKER_CONFIG", child)
            return 0
        with patch.dict(os.environ, env, clear=True), patch.object(docker_ci, "gate", side_effect=gate):
            self.assertEqual(docker_ci.main(), 0)
        env["TERRARIUM_DOCKER_HOST"] = "ssh://untrusted"
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(docker_ci.main(), 1)
        # An inherited DOCKER_HOST alone is not the daemon's job endpoint.
        del env["TERRARIUM_DOCKER_HOST"]
        env["DOCKER_HOST"] = endpoint
        with patch.dict(os.environ, env, clear=True), patch.object(docker_ci, "gate") as gate:
            self.assertEqual(docker_ci.main(), 1)
            gate.assert_not_called()


if __name__ == "__main__":
    unittest.main()
