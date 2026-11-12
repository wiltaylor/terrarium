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
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("hot_swap", ROOT / "docker/hot-swap.py")
HOT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HOT)


class HotSwapTests(unittest.TestCase):
    def test_supervisor_reports_daemon_exit_one_immediately(self):
        source = (ROOT / "docker/entrypoint.sh").read_text()
        loop = source[source.index('RUN_DIR="'):]
        supervisor = self.path / "crash-supervisor.sh"
        supervisor.write_text('log() { printf "entrypoint: %s\\n" "$*" >&2; }\n' + loop)
        daemon = self.path / "crashing-daemon"
        daemon.write_text('#!/bin/sh\nprintf "Error: FOREIGN KEY constraint failed\\n" >&2\nexit 1\n')
        daemon.chmod(0o700)
        output = self.path / "crash.log"
        with output.open("w") as log:
            process = subprocess.Popen(["bash", str(supervisor), str(daemon)], stderr=log)
        def cleanup():
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)
        self.addCleanup(cleanup)
        started = time.monotonic()
        while "entrypoint: terrariumd ended (status 1)" not in output.read_text():
            self.assertLess(time.monotonic() - started, 3, "daemon exit was not reported promptly")
            time.sleep(.02)
        self.assertIn("Error: FOREIGN KEY constraint failed", output.read_text())

    def test_ready_rejects_non_ok_health(self):
        HOT.main(["init"])
        for status, body in ((200, b'{"status":"activating"}'),
                             (503, b'{"status":"activation_failed"}')):
            connection = MagicMock()
            connection.getresponse.return_value.status = status
            connection.getresponse.return_value.read.return_value = body
            with patch.object(HOT.http.client, "HTTPConnection", return_value=connection), \
                    patch.object(HOT.socket, "socket"):
                with self.assertRaisesRegex(RuntimeError, "not ready"):
                    HOT.main(["ready"])
            connection.close.assert_called_once()

    def test_start_marks_probation_before_selecting_release(self):
        HOT.main(["init"])
        root = self.state / "releases"
        key = "b" * 64
        (root / key).mkdir()
        (self.run / "upgrade-paused").touch()
        (self.run / "upgrade-pause").touch()
        HOT.record(root, phase="backed_up", old="a" * 64)
        select = HOT.select
        def interrupted(root, release):
            self.assertTrue((root / "probation").exists())
            select(root, release)
            raise RuntimeError("interrupted after selection")
        with patch.object(HOT, "select", side_effect=interrupted):
            with self.assertRaisesRegex(RuntimeError, "interrupted"):
                HOT.main(["start", key])
        with self.assertRaisesRegex(RuntimeError, "rollback in progress"):
            HOT.main(["cancel-pause"])
        self.assertEqual(HOT.transaction(root)["phase"], "backed_up")
        self.assertTrue((self.run / "upgrade-pause").exists())

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

    def test_interrupted_rollback_pause_preserves_phase_and_refuses_cancel(self):
        HOT.main(["init"])
        root = self.state / "releases"
        (self.run / "upgrade-supervisor.pid").write_text("123")
        (self.run / "upgrade-paused").touch()
        (root / "probation").touch()
        for phase in ("probation", "restoring", "restored"):
            HOT.record(root, phase=phase, admission_attempted=False, old="a" * 64)
            with patch.object(HOT.os, "kill"):
                HOT.main(["pause"])
            self.assertEqual(HOT.transaction(root)["phase"], phase)
            with self.assertRaisesRegex(RuntimeError, "rollback in progress"):
                HOT.main(["cancel-pause"])
            self.assertTrue((self.run / "upgrade-pause").exists())
            self.assertEqual(HOT.transaction(root)["phase"], phase)

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
        # Default CI uses a small fixture. The production-size card check sets
        # TERRARIUM_HOT_SWAP_SEED_MIB=2048 on the build target. SQLite allocates
        # real pages, not a sparse file. Copy cost scales with DB bytes, not
        # the size of the agent directories or logs in the full ZIP.
        db = self.state / "terrarium.db"
        seed_mib = int(os.environ.get("TERRARIUM_HOT_SWAP_SEED_MIB", "1"))
        with sqlite3.connect(db) as conn:
            conn.execute("CREATE TABLE payload(value BLOB)")
            for _ in range(seed_mib):
                conn.execute("INSERT INTO payload VALUES (zeroblob(1048576))")
            conn.execute("CREATE TABLE journal(value TEXT)")
            conn.execute("INSERT INTO journal VALUES ('before-backup')")
        # Represent the write between the online ZIP and the pause. Automatic
        # rollback must retain it, unlike manual restore of the earlier ZIP.
        with sqlite3.connect(db) as conn:
            conn.execute("INSERT INTO journal VALUES ('after-backup-before-pause')")
        state_mib = int(os.environ.get("TERRARIUM_HOT_SWAP_STATE_MIB", "0"))
        data = b'x' * 1048576
        for n in range(state_mib):
            directory = self.state / ("agents/A1/logs" if n % 2 else "archive")
            directory.mkdir(parents=True, exist_ok=True)
            (directory / f"{n}.log").write_bytes(data)
        started = time.monotonic()
        (self.run / "upgrade-pause").touch()
        process.send_signal(signal.SIGUSR1)
        wait(lambda: (self.run / "upgrade-paused").exists())
        self.assertIsNone(process.poll())
        self.assertEqual(len(log.read_text().splitlines()), 1)
        backup = str(self.state / "pre-update.zip")
        copy_started = time.monotonic()
        HOT.main(["snapshot", backup])
        copy_seconds = time.monotonic() - copy_started
        (self.run / "upgrade-pause").unlink()
        wait(lambda: len(log.read_text().splitlines()) == 2)
        pause_seconds = time.monotonic() - started
        self.assertLess(pause_seconds, 30, "paused snapshot and restart exceeded 30 seconds")
        # Failed-new-release rollback is a separate pause, not part of the
        # successful swap latency. Still verify the exact paused state and
        # report recovery time without conflating these two paths.
        with sqlite3.connect(db) as conn:
            conn.execute("DELETE FROM journal")
            conn.execute("INSERT INTO journal VALUES ('new-release-write')")
        recovery_started = time.monotonic()
        HOT.main(["pause", "5"])
        restore_started = time.monotonic()
        HOT.main(["restore-database", backup])
        restore_seconds = time.monotonic() - restore_started
        with sqlite3.connect(db) as conn:
            self.assertEqual(conn.execute("SELECT value FROM journal ORDER BY value").fetchall(),
                             [("after-backup-before-pause",), ("before-backup",)])
        (self.run / "upgrade-pause").unlink()
        wait(lambda: len(log.read_text().splitlines()) == 3)
        recovery_seconds = time.monotonic() - recovery_started
        rollback_hold_seconds = time.monotonic() - started
        # Match the updater's 15-minute hold minus its one-minute safety
        # margin, counted from the original pause, not a fresh recovery timer.
        self.assertLess(rollback_hold_seconds, 15 * 60 - 60,
                        "rollback exceeded the runner hold safety deadline")
        print(f"hot-swap seed_bytes={db.stat().st_size} state_file_bytes={state_mib * 1048576} "
              f"snapshot_seconds={copy_seconds:.3f} "
              f"pause_seconds={pause_seconds:.3f} restore_seconds={restore_seconds:.3f} "
              f"rollback_pause_seconds={recovery_seconds:.3f} "
              f"rollback_hold_seconds={rollback_hold_seconds:.3f}")
        self.assertEqual(len(set(log.read_text().splitlines())), 3)
        HOT.main(["commit", backup])

        # Force a timeout after SIGUSR1. Cleanup must request a restart rather
        # than letting the later clean daemon exit stop the whole container.
        with patch.object(HOT, "wait_for", side_effect=TimeoutError):
            with self.assertRaises(TimeoutError):
                HOT.main(["pause"])
        HOT.main(["cancel-pause", "5"])
        wait(lambda: len(log.read_text().splitlines()) == 4)
        self.assertIsNone(process.poll())
        self.assertFalse((self.run / "upgrade-pause").exists())


if __name__ == "__main__":
    unittest.main()
