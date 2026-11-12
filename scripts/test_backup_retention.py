"""Retention never operates on production paths in these tests."""
import datetime as dt
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch
import zipfile

SPEC = importlib.util.spec_from_file_location("hot", Path(__file__).resolve().parents[1] / "docker/hot-swap.py")
HOT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HOT)
NOW = dt.datetime(2026, 11, 12, 20, tzinfo=dt.timezone.utc)


class RetentionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.state = Path(self.tmp.name)
        self.directory = self.state / "backups"
        self.directory.mkdir()
        env = patch.dict(os.environ, {"TERRARIUM_STATE_DIR": str(self.state), "TERRARIUM_BACKUP_DIR": str(self.directory)})
        env.start()
        self.addCleanup(env.stop)

    def backup(self, age, hour=12, kind="full", base=None):
        created = (NOW - dt.timedelta(days=age)).replace(hour=hour)
        name = f"terrarium-backup-{created:%Y%m%dT%H%M%SZ}-{kind}.zip"
        with zipfile.ZipFile(self.directory / name, "w") as archive:
            archive.writestr("backup.json", json.dumps({"format": "terrarium-backup", "version": 1,
                "kind": kind, "created_at": created.isoformat(), "base": {"name": base}}))
        return name

    def test_newest_five_plus_latest_per_utc_day_for_fourteen_days(self):
        names = [self.backup(age, hour) for age in range(20) for hour in (8, 12, 16)]
        protected = self.backup(30)
        snapshot = self.directory / (names[-1] + ".hot-swap.sqlite")
        snapshot.write_bytes(b"snapshot")
        all_full = sorted(names, reverse=True)
        keep = set(all_full[:5]) | {name for name in names if "T160000Z" in name and name >= "terrarium-backup-20261030"} | {protected}
        # Multiple bounded passes drain the eligible set.
        for _ in range(8):
            result = HOT.retain_backups([str(self.directory / protected)], NOW)
            self.assertLessEqual(len(result["removed"]), 32)
        self.assertEqual({p.name for p in self.directory.glob("*.zip")}, keep)
        self.assertFalse(snapshot.exists())
        journal = [json.loads(line) for line in (self.directory / ".retention-audit.jsonl").read_text().splitlines()]
        deleted = [r for r in journal if r["action"] == "backup.prune"]
        self.assertEqual(len(deleted), len(names) - len(keep) + 2)
        self.assertTrue(all(isinstance(r["size"], int) for r in deleted))

    def test_diff_bases_unknown_files_and_future_backups_survive(self):
        base = self.backup(40)
        self.backup(1, kind="diff", base=base)
        future = self.backup(-2)
        for age in range(8):
            self.backup(age)
        unknown = self.directory / "manual.zip"
        unknown.write_bytes(b"not ours")
        HOT.retain_backups([], NOW)
        self.assertTrue((self.directory / base).exists())
        self.assertTrue((self.directory / future).exists())
        self.assertEqual(unknown.read_bytes(), b"not ours")

    def test_symlink_never_deletes_outside_directory(self):
        outside = self.state / "outside"
        outside.write_bytes(b"keep")
        (self.directory / "terrarium-backup-20200101T000000Z-full.zip").symlink_to(outside)
        with self.assertRaises(OSError):
            HOT.retain_backups([], NOW)
        self.assertEqual(outside.read_bytes(), b"keep")

    def test_creator_flock_serializes_retention(self):
        for age in range(20):
            self.backup(age)
        started = threading.Event()
        finished = threading.Event()
        errors = []
        def retain():
            started.set()
            try:
                HOT.retain_backups([], NOW)
            except Exception as e:
                errors.append(e)
            finally:
                finished.set()
        with (self.directory / ".retention.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            worker = threading.Thread(target=retain)
            worker.start()
            self.assertTrue(started.wait(2))
            self.assertFalse(finished.wait(.1))
            self.assertEqual(len(list(self.directory.glob("*.zip"))), 20)
        worker.join(5)
        self.assertTrue(finished.is_set())
        self.assertEqual(errors, [])


if __name__ == "__main__":
    unittest.main()
