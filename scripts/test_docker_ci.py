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
        with patch.dict(os.environ, {"TERRARIUM_JOB_KIND": "validate", "TERRARIUM_DOCKER_CI": "1", docker_ci.KEY_ENV: "dummy"}):
            with patch.object(docker_ci, "gate") as gate:
                self.assertEqual(docker_ci.main(), 1)
                gate.assert_not_called()

    def test_missing_key_is_a_failure(self):
        with patch.dict(os.environ, {"TERRARIUM_JOB_KIND": "merge", "TERRARIUM_DOCKER_CI": "1"}, clear=True):
            self.assertEqual(docker_ci.main(), 1)

    def test_default_path_needs_no_key_or_docker_and_keeps_full_validation(self):
        for switch in (None, "0"):
            env = {"TERRARIUM_JOB_KIND": "merge", docker_ci.KEY_ENV: "dummy"}
            if switch is not None:
                env["TERRARIUM_DOCKER_CI"] = switch
            with patch.dict(os.environ, env, clear=True), \
                 patch.object(subprocess, "call", return_value=17) as validate, \
                 patch.object(subprocess, "Popen") as agent, \
                 patch.object(docker_ci, "gate") as gate:
                self.assertEqual(docker_ci.main(), 17)
                validate.assert_called_once_with(["just", "ci::validate"], env={
                    k: v for k, v in env.items() if k != docker_ci.KEY_ENV})
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

    def test_ssh_key_is_stdin_only_and_agent_is_cleaned_up(self):
        env_seen = []
        class Agent:
            def __init__(self, cmd, **kwargs):
                Path(cmd[-1]).touch()
                self.stopped = False
            def terminate(self):
                self.stopped = True
            def wait(self, **kwargs):
                self.assert_stopped = self.stopped
                return 0
        agent = []
        def start(cmd, **kwargs):
            value = Agent(cmd, **kwargs)
            agent.append(value)
            return value
        def add(cmd, **kwargs):
            self.assertEqual(cmd, ["ssh-add", "-"])
            self.assertEqual(kwargs["input"], "dummy-key\n")
            self.assertNotIn(docker_ci.KEY_ENV, kwargs["env"])
            return subprocess.CompletedProcess(cmd, 0)
        def gate(env):
            env_seen.append(env)
            self.assertEqual(env["DOCKER_HOST"], docker_ci.DOCKER_HOST)
            self.assertNotIn(docker_ci.KEY_ENV, env)
            self.assertNotIn("DOCKER_CONTEXT", env)
            self.assertNotIn("DOCKER_TLS_VERIFY", env)
            temp = Path(env["SSH_AUTH_SOCK"]).parent
            self.assertEqual((temp / "known_hosts").read_text(), docker_ci.KNOWN_HOST)
            self.assertIn("StrictHostKeyChecking=yes", (temp / "ssh").read_text())
            self.assertEqual({p.name for p in temp.iterdir()}, {"ssh", "known_hosts", "agent.sock"})
            raise RuntimeError("test failure after setup")
        with patch.dict(os.environ, {"TERRARIUM_JOB_KIND": "merge", "TERRARIUM_DOCKER_CI": "1", docker_ci.KEY_ENV: "dummy-key",
                                     "DOCKER_CONTEXT": "wrong-daemon", "DOCKER_TLS_VERIFY": "1"}):
            with patch.object(subprocess, "Popen", side_effect=start), \
                 patch.object(subprocess, "run", side_effect=add), \
                 patch.object(docker_ci.shutil, "which", return_value="/usr/bin/ssh"), \
                 patch.object(docker_ci, "gate", side_effect=gate):
                with self.assertRaisesRegex(RuntimeError, "test failure after setup"):
                    docker_ci.main()
        self.assertTrue(agent[0].stopped)
        self.assertEqual(len(env_seen), 1)


if __name__ == "__main__":
    unittest.main()
