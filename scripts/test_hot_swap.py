"""Filesystem and supervisor contract tests, with scratch processes only."""
import importlib.util
import os
import re
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("hot_swap", ROOT / "docker/hot-swap.py")
HOT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HOT)


class HotSwapTests(unittest.TestCase):
    def test_control_helper_is_in_the_docker_build_context(self):
        rules = (ROOT / "docker/Dockerfile.dockerignore").read_text().splitlines()
        self.assertIn("!docker/hot-swap.py", rules)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name)
        self.state = self.path / "state"
        self.run = self.path / "run"
        self.payload = self.path / "payload"
        for p in (self.state, self.run, self.payload):
            p.mkdir()
        (self.payload / "release-id").write_text("a" * 64)
        (self.payload / "binary").write_text("immutable")
        self.env = patch.dict(os.environ, {
            "TERRARIUM_STATE_DIR": str(self.state),
            "TERRARIUM_RUNTIME_DIR": str(self.run),
            "TERRARIUM_BUNDLED_RELEASE": str(self.payload),
            "TERRARIUM_DAEMON_SERVICE": "1",
        })
        self.env.start()
        self.addCleanup(self.env.stop)

    def test_container_restart_preserves_selection_replacement_selects_bundled(self):
        HOT.main(["init"])
        releases = self.state / "releases"
        other = releases / ("b" * 64)
        other.mkdir()
        HOT.select(releases, other)
        HOT.main(["init"])
        self.assertEqual((releases / "current").resolve(), other)
        (self.run / "upgrade-initialized").unlink()
        (releases / ("a" * 64) / "image.json").write_text('{"reference":"stale-base"}')
        HOT.main(["init"])
        self.assertEqual((releases / "current").resolve().name, "a" * 64)
        self.assertFalse((releases / "current/image.json").exists())

    def test_database_rollback_preserves_runner_files_and_probation(self):
        HOT.main(["init"])
        db = self.state / "terrarium.db"
        with sqlite3.connect(db) as conn:
            conn.execute("CREATE TABLE journal(id TEXT PRIMARY KEY)")
            conn.execute("INSERT INTO journal VALUES ('answered')")
        backup = str(self.state / "backup")
        HOT.main(["snapshot", backup])
        with sqlite3.connect(db) as conn:
            conn.execute("CREATE TABLE migrated(value TEXT)")
        runner = self.state / "runner.log"
        runner.write_text("events after snapshot")
        probation = self.state / "releases/probation"
        probation.touch()
        HOT.main(["restore-database", backup])
        with sqlite3.connect(db) as conn:
            self.assertEqual(conn.execute("SELECT id FROM journal").fetchall(), [("answered",)])
            self.assertEqual(conn.execute("SELECT name FROM sqlite_master WHERE name='migrated'").fetchall(), [])
        self.assertEqual(runner.read_text(), "events after snapshot")
        self.assertTrue(probation.exists())
        HOT.main(["commit", backup])
        self.assertFalse(probation.exists())
        self.assertFalse(Path(backup + ".hot-swap.sqlite").exists())

    def test_release_path_cannot_escape_volume(self):
        with self.assertRaises(ValueError):
            HOT.select(self.state, self.path)

    def test_init_requires_service_mode_and_preserves_interrupted_probation(self):
        HOT.main(["init"])
        root = self.state / "releases"
        marker = root / "probation"
        marker.touch()
        (self.run / "upgrade-initialized").unlink()
        with patch.dict(os.environ, {"TERRARIUM_DAEMON_SERVICE": "0"}):
            with self.assertRaises(RuntimeError):
                HOT.main(["init"])
        with self.assertRaises(RuntimeError):
            HOT.main(["init"])
        self.assertTrue(marker.exists())
        self.assertEqual((root / "current").resolve().name, "a" * 64)

    def test_pause_signal_failure_removes_pending_pause(self):
        HOT.main(["init"])
        (self.run / "upgrade-supervisor.pid").write_text("123")
        with patch.object(HOT.os, "kill", side_effect=ProcessLookupError):
            with self.assertRaises(ProcessLookupError):
                HOT.main(["pause"])
        self.assertFalse((self.run / "upgrade-pause").exists())

    def test_commit_is_idempotent_and_retains_current_previous_and_live_payloads(self):
        HOT.main(["init"])
        root = self.state / "releases"
        for key in "bcd":
            (root / (key * 64)).mkdir()
        # c represents a payload still mapped by a surviving runner.
        real_link = HOT.os.readlink
        def link(path):
            if str(path).endswith("/exe"):
                return str(root / ("c" * 64) / "bin/terrarium-runner")
            return real_link(path)
        with patch.object(HOT.os, "readlink", side_effect=link):
            HOT.main(["commit", str(self.state / "backup"), "b" * 64])
            HOT.main(["commit", str(self.state / "backup"), "b" * 64])
        self.assertTrue((root / ("a" * 64)).exists())
        self.assertTrue((root / ("b" * 64)).exists())
        self.assertTrue((root / ("c" * 64)).exists())
        self.assertFalse((root / ("d" * 64)).exists())
        self.assertTrue(HOT.transaction(root)["admission_attempted"])

    def test_image_epoch_tracks_the_oldest_supported_runner_protocol(self):
        protocol = (ROOT / "crates/terrarium-runner/src/control.rs").read_text()
        version = int(re.search(r"PROTOCOL_VERSION: u32 = (\d+)", protocol).group(1))
        image = (ROOT / "docker/Dockerfile").read_text()
        self.assertIn(f'io.terrarium.runner-compat-epoch="{version - 1}"', image)

    def test_interrupted_transaction_retains_recovery_state(self):
        HOT.main(["init"])
        root = self.state / "releases"
        key = "b" * 64
        (root / key).mkdir()
        HOT.main(["begin", key])
        with sqlite3.connect(self.state / "terrarium.db") as conn:
            conn.execute("CREATE TABLE journal(value TEXT)")
        backup = str(self.state / "backup")
        HOT.main(["snapshot", backup])
        self.assertEqual(HOT.transaction(root)["phase"], "backed_up")
        # A fresh helper process recovers from disk, not updater memory.
        result = subprocess.run(["python3", str(ROOT / "docker/hot-swap.py"),
                                 "status"], capture_output=True, text=True, check=True)
        self.assertIn('"backed_up"', result.stdout)
        HOT.main(["commit", backup])
        with self.assertRaises(RuntimeError):
            HOT.main(["restore-database", backup])

    def test_supervisor_pauses_clean_exit_without_stopping_container(self):
        HOT.main(["init"])
        # Run exactly the production supervisor loop, skipping the image's
        # ownership/bootstrap section (which needs Docker's mounted directories).
        source = (ROOT / "docker/entrypoint.sh").read_text()
        loop = source[source.index('RUN_DIR="'):]
        supervisor = self.path / "supervisor.sh"
        supervisor.write_text('log() { :; }\n' + loop)
        daemon = self.path / "terrariumd"
        daemon.write_text('#!/usr/bin/env python3\nimport os, signal, time\n'
                          'signal.signal(signal.SIGTERM, lambda *_: exit(0))\n'
                          'open(os.environ["PID_LOG"], "a").write(str(os.getpid())+"\\n")\n'
                          'while True: time.sleep(.05)\n')
        daemon.chmod(0o700)
        log = self.path / "pids"
        process = subprocess.Popen(["bash", str(supervisor), str(daemon)],
                                   env={**os.environ, "PID_LOG": str(log)})
        def cleanup():
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)
        self.addCleanup(cleanup)
        def wait(check):
            deadline = time.monotonic() + 5
            while not check():
                self.assertLess(time.monotonic(), deadline, "supervisor timed out")
                time.sleep(.02)
        wait(lambda: log.exists() and len(log.read_text().splitlines()) == 1)
        (self.run / "upgrade-pause").touch()
        process.send_signal(signal.SIGUSR1)
        wait(lambda: (self.run / "upgrade-paused").exists())
        self.assertIsNone(process.poll())
        self.assertEqual(len(log.read_text().splitlines()), 1)
        (self.run / "upgrade-pause").unlink()
        wait(lambda: len(log.read_text().splitlines()) == 2)
        self.assertNotEqual(*log.read_text().splitlines())

        # Force a timeout after SIGUSR1. Cleanup must request a restart rather
        # than letting the later clean daemon exit stop the whole container.
        with patch.object(HOT, "wait_for", side_effect=TimeoutError):
            with self.assertRaises(TimeoutError):
                HOT.main(["pause"])
        HOT.main(["cancel-pause", "5"])
        wait(lambda: len(log.read_text().splitlines()) == 3)
        self.assertIsNone(process.poll())
        self.assertFalse((self.run / "upgrade-pause").exists())


if __name__ == "__main__":
    unittest.main()
