"""Filesystem and supervisor contract tests, with scratch processes only."""
import importlib.util
import os
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
        HOT.main(["init"])
        self.assertEqual((releases / "current").resolve().name, "a" * 64)

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
        HOT.main(["commit"])
        self.assertFalse(probation.exists())

    def test_release_path_cannot_escape_volume(self):
        with self.assertRaises(ValueError):
            HOT.select(self.state, self.path)

    def test_supervisor_pauses_clean_exit_without_stopping_container(self):
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


if __name__ == "__main__":
    unittest.main()
